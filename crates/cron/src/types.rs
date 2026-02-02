//! Core data types for the cron scheduling system.

use serde::{Deserialize, Serialize};

/// How a job is scheduled.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CronSchedule {
    /// One-shot: fire once at `at_ms` (epoch millis).
    At { at_ms: u64 },
    /// Fixed interval: fire every `every_ms` millis, optionally anchored.
    Every {
        every_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        anchor_ms: Option<u64>,
    },
    /// Cron expression (5-field standard or 6-field with seconds).
    Cron {
        expr: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
}

/// What happens when a job fires.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CronPayload {
    /// Inject a system event into the main session.
    SystemEvent { text: String },
    /// Run an isolated agent turn.
    AgentTurn {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
        #[serde(default)]
        deliver: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        to: Option<String>,
    },
}

/// Where the job executes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum SessionTarget {
    /// Inject into the main conversation session.
    Main,
    /// Run in an isolated, throwaway session.
    #[default]
    Isolated,
}

/// Outcome of a single job run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RunStatus {
    Ok,
    Error,
    Skipped,
}

/// Mutable runtime state of a job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CronJobState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_run_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_status: Option<RunStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_duration_ms: Option<u64>,
}

/// A scheduled cron job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CronJob {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub delete_after_run: bool,
    pub schedule: CronSchedule,
    pub payload: CronPayload,
    #[serde(default)]
    pub session_target: SessionTarget,
    #[serde(default)]
    pub state: CronJobState,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Record of a completed run, stored in run history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CronRunRecord {
    pub job_id: String,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub status: RunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// Input for creating a new job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronJobCreate {
    pub name: String,
    pub schedule: CronSchedule,
    pub payload: CronPayload,
    #[serde(default)]
    pub session_target: SessionTarget,
    #[serde(default)]
    pub delete_after_run: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Patch for updating an existing job.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CronJobPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<CronSchedule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<CronPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_target: Option<SessionTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_after_run: Option<bool>,
}

/// Summary status of the cron system.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronStatus {
    pub running: bool,
    pub job_count: usize,
    pub enabled_count: usize,
    pub next_run_at_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schedule_roundtrip_at() {
        let s = CronSchedule::At { at_ms: 1234567890 };
        let json = serde_json::to_string(&s).unwrap();
        let back: CronSchedule = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn test_schedule_roundtrip_every() {
        let s = CronSchedule::Every {
            every_ms: 60_000,
            anchor_ms: Some(1000),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: CronSchedule = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn test_schedule_roundtrip_cron() {
        let s = CronSchedule::Cron {
            expr: "0 9 * * *".into(),
            tz: Some("Europe/Paris".into()),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: CronSchedule = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn test_payload_system_event() {
        let p = CronPayload::SystemEvent {
            text: "hello".into(),
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("systemEvent"));
        let back: CronPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn test_payload_agent_turn() {
        let p = CronPayload::AgentTurn {
            message: "check emails".into(),
            model: None,
            timeout_secs: Some(120),
            deliver: true,
            channel: Some("slack".into()),
            to: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: CronPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn test_cronjob_roundtrip() {
        let job = CronJob {
            id: "abc".into(),
            name: "test".into(),
            enabled: true,
            delete_after_run: false,
            schedule: CronSchedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            payload: CronPayload::SystemEvent {
                text: "ping".into(),
            },
            session_target: SessionTarget::Main,
            state: CronJobState::default(),
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        let json = serde_json::to_string(&job).unwrap();
        let back: CronJob = serde_json::from_str(&json).unwrap();
        assert_eq!(job, back);
    }

    #[test]
    fn test_session_target_default_is_isolated() {
        assert_eq!(SessionTarget::default(), SessionTarget::Isolated);
    }

    #[test]
    fn test_run_record_roundtrip() {
        let rec = CronRunRecord {
            job_id: "j1".into(),
            started_at_ms: 1000,
            finished_at_ms: 2000,
            status: RunStatus::Ok,
            error: None,
            duration_ms: 1000,
            output: Some("done".into()),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: CronRunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec, back);
    }

    #[test]
    fn test_job_create_defaults() {
        let json = r#"{
            "name": "test",
            "schedule": { "kind": "at", "at_ms": 1000 },
            "payload": { "kind": "systemEvent", "text": "hi" }
        }"#;
        let create: CronJobCreate = serde_json::from_str(json).unwrap();
        assert!(create.enabled);
        assert!(!create.delete_after_run);
        assert_eq!(create.session_target, SessionTarget::Isolated);
    }

    #[test]
    fn test_cron_status_serialize() {
        let s = CronStatus {
            running: true,
            job_count: 5,
            enabled_count: 3,
            next_run_at_ms: Some(999),
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["running"], true);
        assert_eq!(v["jobCount"], 5);
    }
}
