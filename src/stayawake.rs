use tokio_util::sync::CancellationToken;

#[cfg(windows)]
pub fn start(cancel: CancellationToken) {
    use windows::Win32::System::Power::{
        ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };

    let handle = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        // SAFETY: the call takes flags only and changes this thread's own power request.
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
        }
        handle.block_on(cancel.cancelled());
        // SAFETY: the call takes flags only and clears the request this thread made above.
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
    });
}

#[cfg(not(windows))]
pub fn start(cancel: CancellationToken) {
    let _ = cancel;
}
