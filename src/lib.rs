mod error;
mod option;
mod sleep;
mod timer_strategy;

use std::{
    sync::{Arc, RwLock, atomic::AtomicBool},
    time::Duration,
};

use autd3_core::{
    ethercat::{EC_INPUT_FRAME_SIZE, EC_OUTPUT_FRAME_SIZE},
    link::{AsyncLink, LinkError, RxMessage, TxMessage},
};
use error::EtherCrabError;

use async_channel::{Sender, bounded};
use ethercrab::{
    DcSync, MainDevice, PduStorage, RegisterAddress, std::ethercat_now, subdevice_group::CycleInfo,
};
use sleep::{BusyWait, Sleep, StdSleep};
use ta::{Next, indicators::ExponentialMovingAverage};

pub use ethercrab::{MainDeviceConfig, Timeouts, subdevice_group::DcConfiguration};
pub use option::EtherCrabOption;
pub use timer_strategy::TimerStrategy;
use tokio::{task::JoinHandle, time::Instant};

const MAX_FRAMES: usize = 8;
const MAX_SLAVES: usize = 16;
const MAX_PDU_DATA: usize =
    PduStorage::element_size((EC_OUTPUT_FRAME_SIZE + EC_INPUT_FRAME_SIZE) * MAX_SLAVES);
const PDI_LEN: usize = (EC_OUTPUT_FRAME_SIZE + EC_INPUT_FRAME_SIZE) * MAX_SLAVES;

static PDU_STORAGE: PduStorage<MAX_FRAMES, MAX_PDU_DATA> = PduStorage::new();

struct EtherCrabInner {
    is_open: Arc<AtomicBool>,
    #[cfg(target_os = "windows")]
    tx_rx_task: Option<std::thread::JoinHandle<Result<(), std::io::Error>>>,
    #[cfg(not(target_os = "windows"))]
    tx_rx_task: Option<JoinHandle<Result<(), ethercrab::error::Error>>>,
    task: Option<JoinHandle<Result<(), ethercrab::error::Error>>>,
    sender: Sender<Vec<TxMessage>>,
    interval: Duration,
    inputs: Arc<RwLock<Vec<u8>>>,
}

impl EtherCrabInner {
    async fn open(
        geometry: &autd3_core::geometry::Geometry,
        interface: &str,
        option: &EtherCrabOption,
    ) -> Result<EtherCrabInner, EtherCrabError> {
        let EtherCrabOption {
            buf_size,
            timer_strategy,
            ec_timeouts,
            ec_client_config,
            dc_config,
            interval,
            sync_tolerance,
            sync_timeout,
        } = option;

        let (tx, rx, pdu_loop) = PDU_STORAGE
            .try_split()
            .map_err(|_| EtherCrabError::PduStorageError)?;

        let client = Arc::new(MainDevice::new(pdu_loop, *ec_timeouts, *ec_client_config));

        #[cfg(target_os = "windows")]
        let tx_rx_task = std::thread::spawn({
            let interface = interface.to_string();
            move || ethercrab::std::tx_rx_task_blocking(&interface, tx, rx)
        });
        #[cfg(not(target_os = "windows"))]
        let tx_rx_task = tokio::spawn(ethercrab::std::tx_rx_task(&interface, tx, rx)?);

        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut group = client
            .init_single_group::<MAX_SLAVES, PDI_LEN>(ethercat_now)
            .await?;

        if !group.iter(&client).all(|slave| slave.name() == "AUTD") {
            return Err(EtherCrabError::DeviceNotFound);
        }

        tracing::info!("Found {} AUTD3 devices on {}", group.len(), interface);
        if geometry.len() != group.len() {
            return Err(EtherCrabError::DeviceNumberMismatch(
                geometry.len(),
                group.len(),
            ));
        }

        group.iter(&client).for_each(|mut slave| {
            slave.set_dc_sync(DcSync::Sync0);
        });

        tracing::info!("Moving into PRE-OP with PDI");

        let mut group = group.into_pre_op_pdi(&client).await?;

        tracing::info!("Done. PDI available. Waiting for SubDevices to align");

        let mut averages = vec![ExponentialMovingAverage::new(9).unwrap(); group.len()];
        let mut now = Instant::now();
        let start = Instant::now();
        loop {
            group.tx_rx_sync_system_time(&client).await?;

            if now.elapsed() >= Duration::from_millis(25) {
                now = Instant::now();

                let mut max_deviation = Duration::ZERO;
                for (slave, ema) in group.iter(&client).zip(averages.iter_mut()) {
                    let diff = match slave
                        .register_read::<u32>(RegisterAddress::DcSystemTimeDifference)
                        .await
                    {
                        Ok(value) => {
                            const MASK: u32 = 0x7FFFFFFF;
                            if value & !MASK != 0 {
                                -((value & MASK) as i32)
                            } else {
                                value as i32
                            }
                        }
                        Err(ethercrab::error::Error::WorkingCounter { .. }) => 0,
                        Err(e) => {
                            return Err(e.into());
                        }
                    };
                    let diff = Duration::from_nanos(ema.next(diff as f64).abs() as _);
                    max_deviation = max_deviation.max(diff);
                }

                tracing::debug!("--> Max deviation {:?}", max_deviation);

                if max_deviation < *sync_tolerance {
                    tracing::info!("Clocks settled after {} ms", start.elapsed().as_millis());
                    break;
                }

                if start.elapsed() > *sync_timeout {
                    return Err(EtherCrabError::SyncTimeout(max_deviation));
                }
            }
            tokio::time::sleep_until(now + Duration::from_millis(25)).await;
        }

        tracing::info!("Alignment done");

        let group = group.configure_dc_sync(&client, *dc_config).await?;

        let group = group.into_safe_op(&client).await?;

        tracing::info!("SAFE-OP");

        let mut group = group.request_into_op(&client).await?;

        tracing::info!("OP requested");

        let op_request = Instant::now();

        while !group.all_op(&client).await? {
            let now = Instant::now();

            let (
                _wkc,
                CycleInfo {
                    next_cycle_wait, ..
                },
            ) = group.tx_rx_dc(&client).await?;

            tokio::time::sleep_until(now + next_cycle_wait).await;
        }

        tracing::info!(
            "All SubDevices entered OP in {} us",
            op_request.elapsed().as_micros()
        );

        let is_open = Arc::new(AtomicBool::new(true));
        let (sender, receiver) = bounded::<Vec<TxMessage>>(*buf_size);
        let inputs = Arc::new(RwLock::new(vec![0x00u8; group.len() * EC_INPUT_FRAME_SIZE]));
        let task = tokio::task::spawn_blocking({
            let is_open = is_open.clone();
            let inputs = inputs.clone();
            let timer_strategy = *timer_strategy;
            move || {
                tokio::runtime::Handle::current().block_on(async move {
                    while is_open.load(std::sync::atomic::Ordering::Relaxed) {
                        let now = tokio::time::Instant::now();
                        let (
                            _wkc,
                            CycleInfo {
                                next_cycle_wait, ..
                            },
                        ) = group.tx_rx_dc(&client).await?;

                        let mut inputs_guard = inputs.write().unwrap();
                        group.iter(&client).enumerate().for_each(|(idx, slave)| {
                            let offset = idx * EC_INPUT_FRAME_SIZE;
                            inputs_guard[offset..offset + EC_INPUT_FRAME_SIZE]
                                .copy_from_slice(slave.inputs_raw());
                        });
                        drop(inputs_guard);

                        if let Ok(tx) = receiver.try_recv() {
                            group
                                .iter(&client)
                                .enumerate()
                                .for_each(|(idx, mut slave)| {
                                    let o = slave.outputs_raw_mut();
                                    unsafe {
                                        std::ptr::copy_nonoverlapping(
                                            tx.as_ptr().add(idx) as *const _,
                                            o.as_mut_ptr(),
                                            std::mem::size_of::<TxMessage>(),
                                        );
                                    }
                                });
                        }

                        match timer_strategy {
                            TimerStrategy::Sleep => StdSleep::sleep_until(now + next_cycle_wait),
                            TimerStrategy::BusyWait => BusyWait::sleep_until(now + next_cycle_wait),
                        }
                    }

                    Ok(())
                })
            }
        });

        Ok(EtherCrabInner {
            is_open,
            tx_rx_task: Some(tx_rx_task),
            task: Some(task),
            sender,
            interval: *interval,
            inputs,
        })
    }

    async fn close(&mut self) -> Result<(), LinkError> {
        if !self.is_open() {
            return Ok(());
        }

        while !self.sender.is_empty() {
            tokio::time::sleep(self.interval).await;
        }

        self.is_open
            .store(false, std::sync::atomic::Ordering::Relaxed);

        if let Some(task) = self.task.take() {
            let _ = task.await.expect("Join task");
        }

        if let Some(tx_rx_task) = self.tx_rx_task.take() {
            #[cfg(target_os = "windows")]
            let _ = tx_rx_task.join().expect("Join TX/RX task");
            #[cfg(not(target_os = "windows"))]
            tx_rx_task.abort();
        }

        Ok(())
    }

    async fn send(&mut self, tx: &[TxMessage]) -> Result<(), LinkError> {
        self.sender
            .send(tx.to_vec())
            .await
            .map_err(|_| LinkError::new("channel closed".to_owned()))?;
        Ok(())
    }

    async fn receive(&mut self, rx: &mut [RxMessage]) {
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.inputs.read().unwrap().as_ptr() as *const RxMessage,
                rx.as_mut_ptr(),
                rx.len(),
            );
        }
    }

    fn is_open(&self) -> bool {
        self.is_open.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A [`AsyncLink`] using [ethercrab].
pub struct EtherCrab {
    interface: String,
    option: EtherCrabOption,
    inner: Option<EtherCrabInner>,
    #[cfg(feature = "blocking")]
    runtime: Option<tokio::runtime::Runtime>,
}

impl EtherCrab {
    /// Creates a new [`EtherCrab`].
    pub fn new(interface: &str, option: EtherCrabOption) -> EtherCrab {
        Self {
            interface: interface.to_owned(),
            option,
            inner: None,
            #[cfg(feature = "blocking")]
            runtime: None,
        }
    }
}

#[cfg_attr(feature = "async-trait", autd3_core::async_trait)]
impl AsyncLink for EtherCrab {
    async fn open(&mut self, geometry: &autd3_core::geometry::Geometry) -> Result<(), LinkError> {
        self.inner = Some(EtherCrabInner::open(geometry, &self.interface, &self.option).await?);
        Ok(())
    }

    async fn close(&mut self) -> Result<(), LinkError> {
        if let Some(mut inner) = self.inner.take() {
            inner.close().await?;
        }
        Ok(())
    }

    async fn send(&mut self, tx: &[TxMessage]) -> Result<(), LinkError> {
        if let Some(inner) = self.inner.as_mut() {
            inner.send(tx).await?;
            Ok(())
        } else {
            Err(LinkError::new("Link is closed"))
        }
    }

    async fn receive(&mut self, rx: &mut [RxMessage]) -> Result<(), LinkError> {
        if let Some(inner) = self.inner.as_mut() {
            inner.receive(rx).await;
            Ok(())
        } else {
            Err(LinkError::new("Link is closed"))
        }
    }

    fn is_open(&self) -> bool {
        self.inner.is_some()
    }
}

#[cfg(feature = "blocking")]
use autd3_core::link::Link;

#[cfg_attr(docsrs, doc(cfg(feature = "blocking")))]
#[cfg(feature = "blocking")]
impl Link for EtherCrab {
    fn open(&mut self, geometry: &autd3_core::derive::Geometry) -> Result<(), LinkError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create runtime");
        runtime.block_on(<Self as AsyncLink>::open(self, geometry))?;
        self.runtime = Some(runtime);
        Ok(())
    }

    fn close(&mut self) -> Result<(), LinkError> {
        self.runtime
            .as_ref()
            .map_or(Err(LinkError::new("Link is closed")), |runtime| {
                runtime.block_on(async {
                    if let Some(mut inner) = self.inner.take() {
                        inner.close().await?;
                    }
                    Ok(())
                })
            })
    }

    fn update(&mut self, geometry: &autd3_core::geometry::Geometry) -> Result<(), LinkError> {
        self.runtime
            .as_ref()
            .map_or(Err(LinkError::new("Link is closed")), |runtime| {
                runtime.block_on(async {
                    if let Some(inner) = self.inner.as_mut() {
                        inner.update(geometry).await?;
                    }
                    Ok(())
                })
            })
    }

    fn send(&mut self, tx: &[TxMessage]) -> Result<(), LinkError> {
        self.runtime
            .as_ref()
            .map_or(Err(LinkError::new("Link is closed")), |runtime| {
                runtime.block_on(async {
                    if let Some(inner) = self.inner.as_mut() {
                        inner.send(tx).await?;
                        Ok(())
                    } else {
                        Err(LinkError::new("Link is closed"))
                    }
                })
            })
    }

    fn receive(&mut self, rx: &mut [RxMessage]) -> Result<(), LinkError> {
        self.runtime
            .as_ref()
            .map_or(Err(LinkError::new("Link is closed")), |runtime| {
                runtime.block_on(async {
                    if let Some(inner) = self.inner.as_mut() {
                        inner.receive(rx).await?;
                        Ok(())
                    } else {
                        Err(LinkError::new("Link is closed"))
                    }
                })
            })
    }

    fn is_open(&self) -> bool {
        self.runtime.is_some() && self.inner.is_some()
    }
}
