use chrono::Utc;
use serde::Serialize;
use serde_json::Value;

use crate::settings::AppSettings;
use crate::tasks::TaskView;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerEvent {
    pub event: String,
    pub timestamp: String,
    pub data: Value,
}

impl ServerEvent {
    pub fn new(event: impl Into<String>, data: impl Serialize) -> Self {
        Self {
            event: event.into(),
            timestamp: Utc::now().to_rfc3339(),
            data: serde_json::to_value(data).unwrap_or(Value::Null),
        }
    }

    pub fn task_updated(view: &TaskView) -> Self {
        Self::new("task.updated", view)
    }

    pub fn task_removed(task_id: &str) -> Self {
        Self::new(
            "task.removed",
            serde_json::json!({
                "taskId": task_id,
            }),
        )
    }

    pub fn settings_changed(settings: &AppSettings) -> Self {
        Self::new("settings.changed", settings)
    }

    pub fn library_changed(reason: &str, paths: &[String]) -> Self {
        Self::new(
            "library.changed",
            serde_json::json!({
                "reason": reason,
                "paths": paths,
            }),
        )
    }

    pub fn app_ready(base_url: &str) -> Self {
        Self::new(
            "app.ready",
            serde_json::json!({
                "baseUrl": base_url,
            }),
        )
    }
}

pub fn publish(sender: &tokio::sync::broadcast::Sender<ServerEvent>, event: ServerEvent) {
    let _ = sender.send(event);
}