// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Grid gateway-to-gateway routing filters.
//!
//! Currently provides the `grid_route` inference model routing filter.

pub(crate) mod descriptor;
mod route;

pub use route::GridRouteFilter;
