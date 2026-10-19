use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Failure {
    pub message: String,
    pub exit_code: Option<i64>,
    pub stdout: String,
    pub stderr: String,
}

impl Failure {
    pub fn new(message: impl Into<String>) -> Failure {
        Failure {
            message: message.into(),
            ..Failure::default()
        }
    }

    pub fn detail(&self) -> Value {
        let mut detail = Map::new();
        if let Some(code) = self.exit_code {
            detail.insert("exit_code".into(), json!(code));
        }
        if !self.stdout.is_empty() {
            detail.insert("stdout".into(), json!(self.stdout));
        }
        if !self.stderr.is_empty() {
            detail.insert("stderr".into(), json!(self.stderr));
        }
        Value::Object(detail)
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

#[derive(Debug, Clone, PartialEq)]
pub struct HandlerError {
    pub message: String,
    pub detail: Option<Value>,
}

impl HandlerError {
    pub fn plain(message: impl Into<String>) -> HandlerError {
        HandlerError {
            message: message.into(),
            detail: None,
        }
    }

    pub fn with_detail(message: impl Into<String>, detail: Value) -> HandlerError {
        HandlerError {
            message: message.into(),
            detail: Some(detail),
        }
    }
}

impl From<Failure> for HandlerError {
    fn from(failure: Failure) -> HandlerError {
        let detail = failure.detail();
        HandlerError {
            message: failure.message,
            detail: Some(detail),
        }
    }
}

impl From<String> for HandlerError {
    fn from(message: String) -> HandlerError {
        HandlerError::plain(message)
    }
}

impl From<&str> for HandlerError {
    fn from(message: &str) -> HandlerError {
        HandlerError::plain(message)
    }
}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HandlerError {}
