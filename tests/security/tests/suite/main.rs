// SPDX-License-Identifier: LGPL-3.0-only
// Copyright (c) 2024 Shane Utt

//! Security test suite for Praxis.

mod filter_leakage;
mod forwarded_headers;
mod header_injection;
mod host_header;
mod info_leakage;
mod ip_acl;
mod request_smuggling;
