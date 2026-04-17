// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

#![deny(unsafe_code)]
#![deny(unreachable_pub)]

//! TLS configuration types for the Praxis proxy.

mod client_auth;
mod config;
mod error;
pub mod setup;
pub mod sni;

pub use config::{CaConfig, CertKeyPair, ClientCertMode, ClusterTls, ListenerTls, TlsVersion};
pub use error::TlsError;
