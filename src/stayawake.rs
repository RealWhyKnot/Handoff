use tokio_util::sync::CancellationToken;

#[cfg(windows)]
pub fn start(cancel: CancellationToken) {
    use windows::Win32::System::Power::{
        SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED,
    };

    let handle = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
        }
        handle.block_on(cancel.cancelled());
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
    });
}

#[cfg(not(windows))]
pub fn start(cancel: CancellationToken) {
    let _ = cancel;
}
