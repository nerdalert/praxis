// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! A2A-specific extraction from JSON-RPC request bodies.

use std::collections::HashMap;

use serde_json::Value;

// -----------------------------------------------------------------------------
// A2aMethodFamily
// -----------------------------------------------------------------------------

/// A2A method family classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum A2aMethodFamily {
    /// Message-sending methods (`SendMessage`, `SendStreamingMessage`).
    Message,
    /// Task lifecycle methods (`GetTask`, `ListTasks`, `CancelTask`, `SubscribeToTask`).
    Task,
    /// Push notification configuration methods.
    PushNotification,
    /// Agent card methods (`GetExtendedAgentCard`).
    AgentCard,
    /// Unrecognized method family.
    Unknown,
}

impl A2aMethodFamily {
    /// String representation for headers and filter results.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Task => "task",
            Self::PushNotification => "push_notification",
            Self::AgentCard => "agent_card",
            Self::Unknown => "unknown",
        }
    }
}

// -----------------------------------------------------------------------------
// A2aMethod
// -----------------------------------------------------------------------------

/// A2A method classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum A2aMethod {
    /// Send a message to an agent.
    SendMessage,
    /// Send a streaming message to an agent.
    SendStreamingMessage,
    /// Get the status of a task.
    GetTask,
    /// List tasks.
    ListTasks,
    /// Cancel a running task.
    CancelTask,
    /// Subscribe to task updates via streaming.
    SubscribeToTask,
    /// Create a push notification configuration for a task.
    CreateTaskPushNotificationConfig,
    /// Get a push notification configuration for a task.
    GetTaskPushNotificationConfig,
    /// List push notification configurations for a task.
    ListTaskPushNotificationConfigs,
    /// Delete a push notification configuration for a task.
    DeleteTaskPushNotificationConfig,
    /// Get the extended agent card.
    GetExtendedAgentCard,
    /// An unrecognized A2A method.
    Other(String),
}

impl A2aMethod {
    /// Parse from JSON-RPC method string, applying aliases if configured.
    pub(crate) fn from_method_str(s: &str, aliases: &HashMap<String, String>) -> Self {
        let resolved = aliases.get(s).map_or(s, String::as_str);
        match resolved {
            "SendMessage" => Self::SendMessage,
            "SendStreamingMessage" => Self::SendStreamingMessage,
            "GetTask" => Self::GetTask,
            "ListTasks" => Self::ListTasks,
            "CancelTask" => Self::CancelTask,
            "SubscribeToTask" => Self::SubscribeToTask,
            "CreateTaskPushNotificationConfig" => Self::CreateTaskPushNotificationConfig,
            "GetTaskPushNotificationConfig" => Self::GetTaskPushNotificationConfig,
            "ListTaskPushNotificationConfigs" => Self::ListTaskPushNotificationConfigs,
            "DeleteTaskPushNotificationConfig" => Self::DeleteTaskPushNotificationConfig,
            "GetExtendedAgentCard" => Self::GetExtendedAgentCard,
            other => Self::Other(other.to_owned()),
        }
    }

    /// String representation for headers and filter results.
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::SendMessage => "SendMessage",
            Self::SendStreamingMessage => "SendStreamingMessage",
            Self::GetTask => "GetTask",
            Self::ListTasks => "ListTasks",
            Self::CancelTask => "CancelTask",
            Self::SubscribeToTask => "SubscribeToTask",
            Self::CreateTaskPushNotificationConfig => "CreateTaskPushNotificationConfig",
            Self::GetTaskPushNotificationConfig => "GetTaskPushNotificationConfig",
            Self::ListTaskPushNotificationConfigs => "ListTaskPushNotificationConfigs",
            Self::DeleteTaskPushNotificationConfig => "DeleteTaskPushNotificationConfig",
            Self::GetExtendedAgentCard => "GetExtendedAgentCard",
            Self::Other(s) => s,
        }
    }

    /// The method family this method belongs to.
    pub(crate) fn family(&self) -> A2aMethodFamily {
        match self {
            Self::SendMessage | Self::SendStreamingMessage => A2aMethodFamily::Message,
            Self::GetTask | Self::ListTasks | Self::CancelTask | Self::SubscribeToTask => A2aMethodFamily::Task,
            Self::CreateTaskPushNotificationConfig
            | Self::GetTaskPushNotificationConfig
            | Self::ListTaskPushNotificationConfigs
            | Self::DeleteTaskPushNotificationConfig => A2aMethodFamily::PushNotification,
            Self::GetExtendedAgentCard => A2aMethodFamily::AgentCard,
            Self::Other(_) => A2aMethodFamily::Unknown,
        }
    }

    /// Whether this method uses streaming transport.
    pub(crate) fn is_streaming(&self) -> bool {
        matches!(self, Self::SendStreamingMessage | Self::SubscribeToTask)
    }
}

// -----------------------------------------------------------------------------
// A2aEnvelope
// -----------------------------------------------------------------------------

/// Extracted A2A envelope metadata.
#[derive(Debug, Clone)]
pub(crate) struct A2aEnvelope {
    /// The parsed A2A method.
    pub method: A2aMethod,
    /// Task ID extracted from params, if present.
    pub task_id: Option<String>,
    /// Context ID extracted from params, if present.
    pub context_id: Option<String>,
    /// Whether this method uses streaming transport.
    pub streaming: bool,
}

// -----------------------------------------------------------------------------
// Extraction
// -----------------------------------------------------------------------------

/// Extract A2A-specific metadata from a parsed JSON body.
pub(crate) fn extract_a2a_envelope(
    body: &[u8],
    method_str: &str,
    aliases: &HashMap<String, String>,
) -> A2aEnvelope {
    let method = A2aMethod::from_method_str(method_str, aliases);
    let streaming = method.is_streaming();
    let (task_id, context_id) = extract_task_context(body, &method);

    A2aEnvelope {
        method,
        task_id,
        context_id,
        streaming,
    }
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/// Extract task ID and context ID from request params.
fn extract_task_context(body: &[u8], method: &A2aMethod) -> (Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return (None, None);
    };

    let Some(params) = value.get("params") else {
        return (None, None);
    };

    let task_id = match method {
        A2aMethod::GetTask | A2aMethod::CancelTask | A2aMethod::SubscribeToTask => {
            params.get("id").and_then(|v| v.as_str()).map(str::to_owned)
        },
        A2aMethod::CreateTaskPushNotificationConfig
        | A2aMethod::GetTaskPushNotificationConfig
        | A2aMethod::ListTaskPushNotificationConfigs
        | A2aMethod::DeleteTaskPushNotificationConfig => {
            params.get("taskId").and_then(|v| v.as_str()).map(str::to_owned)
        },
        _ => None,
    };

    let context_id = params.get("contextId").and_then(|v| v.as_str()).map(str::to_owned);

    (task_id, context_id)
}
