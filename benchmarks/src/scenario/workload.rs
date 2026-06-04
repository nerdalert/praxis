// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Traffic pattern definitions for benchmark scenarios.

// -----------------------------------------------------------------------------
// Workload
// -----------------------------------------------------------------------------

/// Traffic pattern for a benchmark scenario.
#[derive(Debug, Clone)]
pub enum Workload {
    /// High-concurrency small GET requests.
    SmallRequests {
        /// Number of concurrent connections.
        concurrency: u32,
    },

    /// Large POST requests.
    LargePayload {
        /// Payload size in bytes.
        body_size: usize,
    },

    /// Large POST requests at high concurrency.
    LargePayloadHighConcurrency {
        /// Number of concurrent connections.
        concurrency: u32,

        /// Payload size for requests in bytes.
        body_size: usize,
    },

    /// High connection count HTTP/1.1 stress test.
    HighConnectionCount {
        /// Number of concurrent connections.
        connections: u32,
    },

    /// Sustained load for leak detection.
    ///
    /// Duration is controlled by the parent [`Scenario`].
    ///
    /// [`Scenario`]: super::Scenario
    Sustained,

    /// Ramp-up from low to high QPS.
    Ramp {
        /// Starting requests per second.
        start_qps: u32,

        /// Ending requests per second.
        end_qps: u32,

        /// Step size between ramp levels.
        step: u32,
    },

    /// Raw TCP throughput via Fortio.
    TcpThroughput,

    /// TCP connection rate (new connection per request).
    TcpConnectionRate,

    /// Small OpenAI-compatible chat completion request to `/v1/chat/completions`.
    LlmdChatSmall {
        /// Number of concurrent workers.
        concurrency: u32,
    },

    /// Large-prompt chat completion request with configurable payload size.
    LlmdChatLargePrompt {
        /// Number of concurrent workers.
        concurrency: u32,

        /// Approximate prompt payload size in bytes.
        prompt_size: usize,
    },

    /// Streaming chat completion request with `"stream": true`.
    LlmdChatStreaming {
        /// Number of concurrent workers.
        concurrency: u32,
    },
}

impl Workload {
    /// Returns `true` if this workload targets an llm-d endpoint.
    pub fn is_llmd(&self) -> bool {
        matches!(
            self,
            Self::LlmdChatSmall { .. } | Self::LlmdChatLargePrompt { .. } | Self::LlmdChatStreaming { .. }
        )
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llmd_chat_small_construction() {
        let w = Workload::LlmdChatSmall { concurrency: 16 };
        assert!(
            matches!(w, Workload::LlmdChatSmall { concurrency: 16 }),
            "should construct LlmdChatSmall with correct concurrency"
        );
    }

    #[test]
    fn llmd_chat_large_prompt_construction() {
        let w = Workload::LlmdChatLargePrompt {
            concurrency: 8,
            prompt_size: 65_536,
        };
        assert!(
            matches!(
                w,
                Workload::LlmdChatLargePrompt {
                    concurrency: 8,
                    prompt_size: 65_536
                }
            ),
            "should construct LlmdChatLargePrompt with correct fields"
        );
    }

    #[test]
    fn llmd_chat_streaming_construction() {
        let w = Workload::LlmdChatStreaming { concurrency: 4 };
        assert!(
            matches!(w, Workload::LlmdChatStreaming { concurrency: 4 }),
            "should construct LlmdChatStreaming with correct concurrency"
        );
    }

    #[test]
    fn is_llmd_returns_true_for_llmd_variants() {
        assert!(
            Workload::LlmdChatSmall { concurrency: 16 }.is_llmd(),
            "LlmdChatSmall should be llmd"
        );
        assert!(
            Workload::LlmdChatLargePrompt {
                concurrency: 8,
                prompt_size: 1024
            }
            .is_llmd(),
            "LlmdChatLargePrompt should be llmd"
        );
        assert!(
            Workload::LlmdChatStreaming { concurrency: 4 }.is_llmd(),
            "LlmdChatStreaming should be llmd"
        );
    }

    #[test]
    fn is_llmd_returns_false_for_generic_variants() {
        assert!(
            !Workload::SmallRequests { concurrency: 100 }.is_llmd(),
            "SmallRequests should not be llmd"
        );
        assert!(!Workload::TcpThroughput.is_llmd(), "TcpThroughput should not be llmd");
    }
}
