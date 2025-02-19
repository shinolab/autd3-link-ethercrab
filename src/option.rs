use std::time::Duration;

use ethercrab::{subdevice_group::DcConfiguration, MainDeviceConfig, Timeouts};

use crate::TimerStrategy;

pub struct EtherCrabOption {
    pub buf_size: usize,
    pub timer_strategy: TimerStrategy,
    pub ec_timeouts: Timeouts,
    pub ec_client_config: MainDeviceConfig,
    pub dc_config: DcConfiguration,
    pub interval: Duration,
    pub sync_tolerance: Duration,
    pub sync_timeout: Duration,
}

impl Default for EtherCrabOption {
    fn default() -> Self {
        Self {
            ec_timeouts: Timeouts {
                wait_loop_delay: Duration::from_millis(5),
                state_transition: Duration::from_secs(10),
                pdu: Duration::from_millis(2000),
                ..Timeouts::default()
            },
            ec_client_config: MainDeviceConfig {
                dc_static_sync_iterations: 10_000,
                ..MainDeviceConfig::default()
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
