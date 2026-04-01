// SPDX-License-Identifier: LGPL-3.0-only
// Copyright (c) 2024 Shane Utt

//! HTTP backends for integration testing.

mod echo;
mod simple;
mod specialized;

pub use echo::{start_echo_backend, start_header_echo_backend, start_uri_echo_backend};
pub use simple::{Backend, RoutedBackend, start_backend, start_backend_v6};
pub use specialized::{start_hop_by_hop_response_backend, start_slow_backend};
