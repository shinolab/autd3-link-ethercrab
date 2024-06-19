mod error;
mod sleep;
mod timer_strategy;

use std::{
    sync::{atomic::AtomicBool, Arc, RwLock},
    time::Duration,
};

use autd3_driver::{
    derive::*,
    ethercat::{EC_INPUT_FRAME_SIZE, EC_OUTPUT_FRAME_SIZE},
    firmware::cpu::{RxMessage, TxDatagram},
    link::{Link, LinkBuilder},
};
use error::EtherCrabError;

use async_channel::{bounded, SendError, Sender};
use ethercrab::{
    slave_group::CycleInfo,
    std::{ethercat_now, tx_rx_task},
    Client, DcSync, PduStorage, RegisterAddress,
};
use sleep::{BusyWait, Sleep, StdSleep};
use ta::{indicators::ExponentialMovingAverage, Next};
use tokio::{task::JoinHandle, time::Instant};

pub use ethercrab::{slave_group::DcConfiguration, ClientConfig, Timeouts};
pub use timer_strategy::TimerStrategy;

pub struct EtherCrab {
    is_open: Arc<AtomicBool>,
    timeout: std::time::Duration,
    rx_tx_task: JoinHandle<Result<(), ethercrab::error::Error>>,
    task: Option<JoinHandle<Result<(), ethercrab::error::Error>>>,
    sender: Sender<TxDatagram>,
    interval: Duration,
    inputs: Arc<RwLock<Vec<u8>>>,
}

const MAX_FRAMES: usize = 32;
const MAX_PDU_DATA: usize = PduStorage::element_size(1100);
const MAX_SLAVES: usize = 16;
const PDI_LEN: usize = (EC_OUTPUT_FRAME_SIZE + EC_INPUT_FRAME_SIZE) * MAX_SLAVES;

static PDU_STORAGE: PduStorage<MAX_FRAMES, MAX_PDU_DATA> = PduStorage::new();

#[derive(Builder)]
pub struct EtherCrabBuilder {
    #[getset]
    buf_size: usize,
    #[getset]
    timer_strategy: TimerStrategy,
    #[getset]
    timeout: std::time::Duration,
    #[getset]
    interface: String,
    #[getset]
    ec_timeouts: Timeouts,
    #[getset]
    ec_client_config: ClientConfig,
    #[getset]
    dc_config: DcConfiguration,
    #[getset]
    interval: Duration,
    #[getset]
    pub(crate) sync_tolerance: Duration,
    #[getset]
    pub(crate) sync_timeout: Duration,
}

impl EtherCrabBuilder {
    async fn open_impl(self, geometry: &Geometry) -> Result<EtherCrab, EtherCrabError> {
        let Self {
            buf_size,
            timer_strategy,
            interface,
            ec_timeouts,
            ec_client_config,
            dc_config,
            interval,
            sync_tolerance,
            sync_timeout,
            ..
        } = self;

        let (tx, rx, pdu_loop) = PDU_STORAGE
            .try_split()
            .map_err(|_| EtherCrabError::PduStorageError)?;

        let client = Arc::new(Client::new(pdu_loop, ec_timeouts, ec_client_config));

        let rx_tx_task = tokio::spawn(tx_rx_task(&interface, tx, rx)?);

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

                if max_deviation < sync_tolerance {
                    tracing::info!("Clocks settled after {} ms", start.elapsed().as_millis());
                    break;
                }

                if start.elapsed() > sync_timeout {
                    return Err(EtherCrabError::SyncTimeout(max_deviation));
                }
            }
            tokio::time::sleep_until(now + Duration::from_millis(25)).await;
        }

        tracing::info!("Alignment done");

        let group = group.configure_dc_sync(&client, dc_config).await?;

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
        let (sender, receiver) = bounded::<TxDatagram>(buf_size);
        let inputs = Arc::new(RwLock::new(vec![0x00u8; group.len() * EC_INPUT_FRAME_SIZE]));
        let task = tokio::task::spawn_blocking({
            let is_open = is_open.clone();
            let inputs = inputs.clone();
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
                                    o.copy_from_slice(tx.data(idx));
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

        Ok(EtherCrab {
            is_open,
            timeout: self.timeout,
            rx_tx_task,
            task: Some(task),
            sender,
            interval,
            inputs,
        })
    }
}

#[cfg_attr(feature = "async-trait", autd3_driver::async_trait)]
impl LinkBuilder for EtherCrabBuilder {
    type L = EtherCrab;

    async fn open(self, geometry: &Geometry) -> Result<Self::L, AUTDInternalError> {
        Ok(self.open_impl(geometry).await?)
    }
}

#[cfg_attr(feature = "async-trait", autd3_driver::async_trait)]
impl Link for EtherCrab {
    async fn close(&mut self) -> Result<(), AUTDInternalError> {
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

        self.rx_tx_task.abort();

        Ok(())
    }

    async fn send(&mut self, tx: &TxDatagram) -> Result<bool, AUTDInternalError> {
        if !self.is_open() {
            return Err(AUTDInternalError::LinkClosed);
        }

        match self.sender.send(tx.clone()).await {
            Err(SendError(..)) => Err(AUTDInternalError::LinkClosed),
            _ => Ok(true),
        }
    }

    async fn receive(&mut self, rx: &mut [RxMessage]) -> Result<bool, AUTDInternalError> {
        if !self.is_open() {
            return Err(AUTDInternalError::LinkClosed);
        }

        unsafe {
            std::ptr::copy_nonoverlapping(
                self.inputs.read().unwrap().as_ptr() as *const RxMessage,
                rx.as_mut_ptr(),
                rx.len(),
            );
        }

        Ok(true)
    }

    fn is_open(&self) -> bool {
        self.is_open.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn timeout(&self) -> std::time::Duration {
        self.timeout
    }
}

impl EtherCrab {
    pub fn builder(interface: impl Into<String>) -> EtherCrabBuilder {
        EtherCrabBuilder {
            timeout: std::time::Duration::ZERO,
            interface: interface.into(),
            ec_timeouts: Timeouts {
                wait_loop_delay: Duration::from_millis(5),
                state_transition: Duration::from_secs(10),
                pdu: Duration::from_millis(2000),
                ..Timeouts::default()
            },
            ec_client_config: ClientConfig {
                dc_static_sync_iterations: 10_000,
                ..ClientConfig::default()
            },
            dc_config: DcConfiguration {
                start_delay: Duration::from_millis(100),
                sync0_period: Duration::from_millis(1),
                sync0_shift: Duration::from_millis(1) / 2,
            },
            interval: Duration::from_millis(1),
            sync_tolerance: Duration::from_micros(1),
            sync_timeout: Duration::from_secs(10),
            buf_size: 32,
            timer_strategy: TimerStrategy::Sleep,
        }
    }
}

impl Drop for EtherCrab {
    fn drop(&mut self) {
        if self.is_open() {
            let _ = self.close();
        }
    }
}
