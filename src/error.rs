use std::time::Duration;

use autd3_driver::error::AUTDInternalError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum EtherCrabError {
    #[error("Can only split once")]
    PduStorageError,
    #[error("No interface is available")]
    NoInterfaceAvailable,
    #[error("No AUTD3 device found")]
    DeviceNotFound,
    #[error("{0}")]
    IoError(#[from] std::io::Error),
    #[error("{0}")]
    EtherCrab(#[from] ethercrab::error::Error),
    #[error("Number of devices specified ({0}) does not match the number found ({1})")]
    DeviceNumberMismatch(usize, usize),
    #[error("Failed to synchronize devices (Max deviation: {0:?})")]
    SyncTimeout(Duration),
}

impl From<EtherCrabError> for AUTDInternalError {
    fn from(val: EtherCrabError) -> AUTDInternalError {
        AUTDInternalError::LinkError(val.to_string())
    }
}
