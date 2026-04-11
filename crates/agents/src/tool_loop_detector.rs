//! Loop detector for repeated identical tool-call failures (issue #658).
//!
//! Tracks a short ring buffer of recent tool-call outcomes and fires an
//! escalating intervention when the model gets stuck calling the same tool
//! with the same arguments (or producing the same error) repeatedly.
//!
//! Two escalation stages:
//! 1. **Nudge** — inject a directive system/user message telling the model
//!    to stop, explain what it was trying to do, and respond in text.
//! 2. **Tool stripping** — on the very next iteration, pass an empty tool
//!    schema list to the LLM so it *physically* cannot emit another tool call.
//!
//! A successful tool call resets both the ring buffer and the escalation
//! stage.

use std::{
    collections::{VecDeque, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
};

use serde_json::Value;

/// Fingerprint of a single tool-call outcome used for loop detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallFingerprint {
    pub tool_name: String,
    pub args_hash: u64,
    /// Hash of the tool error string, `None` on success.
    pub error_hash: Option<u64>,
    /// Raw error string (kept for formatting the intervention message).
    pub error_text: Option<String>,
    /// Raw arguments (kept for formatting the intervention message).
    pub arguments: Value,
}

impl ToolCallFingerprint {
    #[must_use]
    pub fn new(tool_name: &str, arguments: &Value, error: Option<&str>) -> Self {
        let args_hash = hash_value(arguments);
        let error_hash = error.map(hash_str);
        Self {
            tool_name: tool_name.to_string(),
            args_hash,
            error_hash,
            error_text: error.map(String::from),
            arguments: arguments.clone(),
        }
    }

    #[must_use]
    pub fn is_failure(&self) -> bool {
        self.error_hash.is_some()
    }
}

/// Escalation stages for the loop detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InterventionStage {
    /// No intervention active.
    #[default]
    None,
    /// Stage 1 fired: a directive nudge has been injected; the next iteration
    /// still passes the normal tool schemas.
    Nudged,
    /// Stage 2 fired: the next iteration will pass an empty tool list, forcing
    /// a text response. After that one forced-text turn the state returns to
    /// [`InterventionStage::None`].
    StripTools,
}

/// Result of recording a new fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopDetectorAction {
    /// No intervention — continue normally.
    None,
    /// Stage 1: inject a directive intervention message for the next LLM call.
    InjectNudge,
    /// Stage 2: strip tool schemas on the next LLM call.
    StripTools,
}

/// Rolling loop detector.
#[derive(Debug)]
pub struct ToolLoopDetector {
    recent: VecDeque<ToolCallFingerprint>,
    window: usize,
    strip_on_second_fire: bool,
    stage: InterventionStage,
}

impl ToolLoopDetector {
    /// Create a new detector with the given window size. `window == 0`
    /// disables detection entirely.
    #[must_use]
    pub fn new(window: usize, strip_on_second_fire: bool) -> Self {
        Self {
            recent: VecDeque::with_capacity(window.max(1)),
            window,
            strip_on_second_fire,
            stage: InterventionStage::None,
        }
    }

    #[must_use]
    pub fn stage(&self) -> InterventionStage {
        self.stage
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.window > 0
    }

    /// Reset state after a successful tool call.
    pub fn reset(&mut self) {
        self.recent.clear();
        self.stage = InterventionStage::None;
    }

    /// Record a tool-call outcome and compute the next action.
    ///
    /// Returns:
    /// - `None` when no intervention should fire.
    /// - `InjectNudge` on the first fire (stage 1).
    /// - `StripTools` on the second consecutive fire, if enabled.
    pub fn record(&mut self, fp: ToolCallFingerprint) -> LoopDetectorAction {
        if self.window == 0 {
            return LoopDetectorAction::None;
        }

        // Success anywhere resets everything.
        if !fp.is_failure() {
            self.reset();
            return LoopDetectorAction::None;
        }

        self.recent.push_back(fp);
        while self.recent.len() > self.window {
            self.recent.pop_front();
        }

        if self.recent.len() < self.window {
            return LoopDetectorAction::None;
        }

        // All entries are failures (we only push failures past the success reset above).
        // Check for identity: same tool + (same args OR same error).
        if !self.all_match() {
            return LoopDetectorAction::None;
        }

        match self.stage {
            InterventionStage::None => {
                self.stage = InterventionStage::Nudged;
                LoopDetectorAction::InjectNudge
            },
            InterventionStage::Nudged if self.strip_on_second_fire => {
                self.stage = InterventionStage::StripTools;
                LoopDetectorAction::StripTools
            },
            InterventionStage::Nudged | InterventionStage::StripTools => {
                // Already escalated — don't re-fire.
                LoopDetectorAction::None
            },
        }
    }

    /// Called by the runner once the post-strip iteration has run. Fully
    /// resets the detector so the next window starts fresh.
    ///
    /// Clearing only the stage but not the ring buffer would leave the deque
    /// still full of `window` matching failures. A single new identical
    /// failure after tools are restored would immediately re-fire stage 2
    /// (`stage: Nudged` + `strip_on_second_fire: true`), oscillating between
    /// strip-tools and normal turns until `max_iterations` — giving the model
    /// almost no runway after the first escalation cycle. Treat the forced
    /// text turn as a hard reset of the detector state.
    pub fn clear_strip_tools(&mut self) {
        if self.stage == InterventionStage::StripTools {
            self.stage = InterventionStage::None;
            self.recent.clear();
        }
    }

    /// Returns a snapshot of the window used for formatting intervention
    /// messages. Callers get a cloned vec so they can format freely.
    #[must_use]
    pub fn window_snapshot(&self) -> Vec<ToolCallFingerprint> {
        self.recent.iter().cloned().collect()
    }

    fn all_match(&self) -> bool {
        let Some(first) = self.recent.front() else {
            return false;
        };
        let all_same_tool = self.recent.iter().all(|fp| fp.tool_name == first.tool_name);
        if !all_same_tool {
            return false;
        }
        let all_same_args = self.recent.iter().all(|fp| fp.args_hash == first.args_hash);
        let all_same_error = first.error_hash.is_some()
            && self
                .recent
                .iter()
                .all(|fp| fp.error_hash == first.error_hash);
        all_same_args || all_same_error
    }
}

/// Build the stage-1 nudge intervention message from the current window.
#[must_use]
pub fn format_intervention_message(window: &[ToolCallFingerprint]) -> String {
    let mut msg = String::from("SYSTEM INTERVENTION — LOOP DETECTED\n\nYour last ");
    msg.push_str(&window.len().to_string());
    msg.push_str(" tool calls were:\n");
    for (i, fp) in window.iter().enumerate() {
        let args_str = serde_json::to_string(&fp.arguments).unwrap_or_else(|_| "{}".to_string());
        let err = fp.error_text.as_deref().unwrap_or("(no error)");
        msg.push_str(&format!(
            "  {}. {}({}) → error: {}\n",
            i + 1,
            fp.tool_name,
            args_str,
            err
        ));
    }

    let tool_name = window
        .first()
        .map(|fp| fp.tool_name.as_str())
        .unwrap_or("this tool");

    msg.push_str(
        "\nThese are identical failed invocations. Retrying with the same arguments will fail \
         again.\n\nOn your next turn:\n",
    );
    msg.push_str(&format!(
        "1. Do NOT call `{tool_name}` or any other tool.\n"
    ));
    msg.push_str("2. Do NOT repeat this call pattern.\n");
    msg.push_str("3. Respond to the user in plain text.\n");
    msg.push_str("4. Explain what you were trying to accomplish.\n");
    msg.push_str("5. If you do not know what arguments to use, ask the user for clarification.\n");
    msg.push_str("\nThe user is waiting for a text response.");
    msg
}

/// Stage-2 reinforcement message used when the runner strips tool schemas for
/// the next iteration. Kept short because the model is forced into text mode
/// regardless.
///
/// Returns `String` (not `&'static str`) so callers can treat it uniformly
/// with [`format_intervention_message`].
#[must_use]
pub fn format_strip_tools_message() -> String {
    "SYSTEM INTERVENTION — TOOLS DISABLED FOR THIS TURN\n\nYou have been caught in a reflex \
     retry loop. Tools are disabled for this single turn. Respond to the user in plain text: \
     explain what you were trying to do, and ask for clarification if needed."
        .to_string()
}

fn hash_value(v: &Value) -> u64 {
    // Canonicalize by serializing; serde_json already sorts object keys
    // deterministically within a single `to_string` call only if the input was
    // a Map<String,_>. serde_json::Map preserves insertion order, so to get a
    // stable fingerprint we walk the value recursively.
    let canonical = canonicalize(v);
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    hasher.finish()
}

fn hash_str(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

fn canonicalize(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("\"{s}\""),
        Value::Array(arr) => {
            let inner: Vec<String> = arr.iter().map(canonicalize).collect();
            format!("[{}]", inner.join(","))
        },
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .into_iter()
                .map(|k| format!("\"{}\":{}", k, canonicalize(&map[k])))
                .collect();
            format!("{{{}}}", inner.join(","))
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use {super::*, serde_json::json};

    fn fp(tool: &str, args: Value, err: Option<&str>) -> ToolCallFingerprint {
        ToolCallFingerprint::new(tool, &args, err)
    }

    #[test]
    fn window_zero_disables_detection() {
        let mut d = ToolLoopDetector::new(0, true);
        assert!(matches!(
            d.record(fp("exec", json!({}), Some("missing"))),
            LoopDetectorAction::None
        ));
        assert!(matches!(
            d.record(fp("exec", json!({}), Some("missing"))),
            LoopDetectorAction::None
        ));
        assert!(matches!(
            d.record(fp("exec", json!({}), Some("missing"))),
            LoopDetectorAction::None
        ));
    }

    #[test]
    fn three_identical_failures_fire_nudge() {
        let mut d = ToolLoopDetector::new(3, true);
        assert_eq!(
            d.record(fp("exec", json!({}), Some("missing 'command'"))),
            LoopDetectorAction::None
        );
        assert_eq!(
            d.record(fp("exec", json!({}), Some("missing 'command'"))),
            LoopDetectorAction::None
        );
        assert_eq!(
            d.record(fp("exec", json!({}), Some("missing 'command'"))),
            LoopDetectorAction::InjectNudge
        );
        assert_eq!(d.stage(), InterventionStage::Nudged);
    }

    #[test]
    fn fourth_failure_after_nudge_strips_tools() {
        let mut d = ToolLoopDetector::new(3, true);
        for _ in 0..3 {
            let _ = d.record(fp("exec", json!({}), Some("missing")));
        }
        assert_eq!(d.stage(), InterventionStage::Nudged);
        assert_eq!(
            d.record(fp("exec", json!({}), Some("missing"))),
            LoopDetectorAction::StripTools
        );
        assert_eq!(d.stage(), InterventionStage::StripTools);
    }

    #[test]
    fn strip_tools_disabled_stays_in_nudged() {
        let mut d = ToolLoopDetector::new(3, false);
        for _ in 0..3 {
            let _ = d.record(fp("exec", json!({}), Some("missing")));
        }
        assert_eq!(
            d.record(fp("exec", json!({}), Some("missing"))),
            LoopDetectorAction::None
        );
        assert_eq!(d.stage(), InterventionStage::Nudged);
    }

    #[test]
    fn success_resets_state() {
        let mut d = ToolLoopDetector::new(3, true);
        for _ in 0..2 {
            let _ = d.record(fp("exec", json!({}), Some("missing")));
        }
        let _ = d.record(fp("exec", json!({"command": "ls"}), None));
        assert_eq!(d.stage(), InterventionStage::None);

        // Need 3 more failures to fire.
        assert_eq!(
            d.record(fp("exec", json!({}), Some("missing"))),
            LoopDetectorAction::None
        );
        assert_eq!(
            d.record(fp("exec", json!({}), Some("missing"))),
            LoopDetectorAction::None
        );
        assert_eq!(
            d.record(fp("exec", json!({}), Some("missing"))),
            LoopDetectorAction::InjectNudge
        );
    }

    #[test]
    fn different_args_same_tool_same_error_still_fires() {
        // Same tool + same error text, different args. Should still fire because
        // `all_match` accepts "all same error".
        let mut d = ToolLoopDetector::new(3, true);
        let err = Some("missing 'command' parameter");
        let _ = d.record(fp("exec", json!({}), err));
        let _ = d.record(fp("exec", json!({"cmd": ""}), err));
        assert_eq!(
            d.record(fp("exec", json!({"cmd": " "}), err)),
            LoopDetectorAction::InjectNudge
        );
    }

    #[test]
    fn different_tools_do_not_fire() {
        let mut d = ToolLoopDetector::new(3, true);
        let _ = d.record(fp("exec", json!({}), Some("e")));
        let _ = d.record(fp("browser", json!({}), Some("e")));
        assert_eq!(
            d.record(fp("exec", json!({}), Some("e"))),
            LoopDetectorAction::None
        );
    }

    #[test]
    fn legitimate_retry_pattern_does_not_fire() {
        // Fail → retry with new args → succeed. This should NOT fire.
        let mut d = ToolLoopDetector::new(3, true);
        let _ = d.record(fp("exec", json!({"command": "ls"}), Some("no such dir")));
        let _ = d.record(fp("exec", json!({"command": "ls /tmp"}), None));
        assert_eq!(d.stage(), InterventionStage::None);
    }

    #[test]
    fn clear_strip_tools_resets_state_fully() {
        let mut d = ToolLoopDetector::new(3, true);
        for _ in 0..3 {
            let _ = d.record(fp("exec", json!({}), Some("missing")));
        }
        let _ = d.record(fp("exec", json!({}), Some("missing")));
        assert_eq!(d.stage(), InterventionStage::StripTools);
        d.clear_strip_tools();
        // A hard reset — not just a stage transition — so the next reflex
        // failure after tools are restored cannot immediately re-fire
        // stage 2 with a still-full deque (the oscillation Greptile flagged).
        assert_eq!(d.stage(), InterventionStage::None);
        assert!(d.window_snapshot().is_empty());
    }

    #[test]
    fn post_strip_single_failure_does_not_immediately_refire() {
        // Regression: after stage 2 has fired and the runner calls
        // clear_strip_tools(), a single identical failure must NOT jump
        // straight back to StripTools. It should take another `window` fresh
        // failures to fire stage 1 again.
        let mut d = ToolLoopDetector::new(3, true);
        // Build up and fire both stages.
        for _ in 0..3 {
            let _ = d.record(fp("exec", json!({}), Some("missing")));
        }
        assert_eq!(d.stage(), InterventionStage::Nudged);
        let _ = d.record(fp("exec", json!({}), Some("missing")));
        assert_eq!(d.stage(), InterventionStage::StripTools);

        // Runner processes the forced-text turn and resets state.
        d.clear_strip_tools();

        // One fresh failure must not re-escalate.
        assert_eq!(
            d.record(fp("exec", json!({}), Some("missing"))),
            LoopDetectorAction::None
        );
        assert_eq!(d.stage(), InterventionStage::None);
        assert_eq!(
            d.record(fp("exec", json!({}), Some("missing"))),
            LoopDetectorAction::None
        );
        // Third identical failure since reset → fresh nudge, not StripTools.
        assert_eq!(
            d.record(fp("exec", json!({}), Some("missing"))),
            LoopDetectorAction::InjectNudge
        );
        assert_eq!(d.stage(), InterventionStage::Nudged);
    }

    #[test]
    fn canonicalize_is_order_stable() {
        let a = json!({"a": 1, "b": 2});
        let b = json!({"b": 2, "a": 1});
        assert_eq!(hash_value(&a), hash_value(&b));
    }

    #[test]
    fn intervention_message_contains_evidence() {
        let window = vec![
            fp("exec", json!({}), Some("missing 'command'")),
            fp("exec", json!({}), Some("missing 'command'")),
            fp("exec", json!({}), Some("missing 'command'")),
        ];
        let msg = format_intervention_message(&window);
        assert!(msg.contains("LOOP DETECTED"));
        assert!(msg.contains("exec"));
        assert!(msg.contains("missing 'command'"));
        assert!(msg.contains("Do NOT"));
        assert!(msg.contains("plain text"));
    }
}
