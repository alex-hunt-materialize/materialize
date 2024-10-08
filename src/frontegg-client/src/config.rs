// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! This module defines the configuration and builder structs for the Frontegg
//! [`Client`].
//!
//! The main type exported from this module is the [`ClientBuilder`] struct,
//! which is used to configure and build instances of the [`Client`] struct. The
//! [`Client`] struct provides methods for interacting with various admin APIs,
//! such as creating and managing users, or listing passwords.
//!
//! # Note
//!
//! This module default endpoint is intended to run against Materialize and is
//! not guaranteed to work for other services.

use std::sync::LazyLock;
use std::time::Duration;

use reqwest::Url;

use crate::client::{Authentication, Client};

/// The default endpoint the client will use to issue the requests. Currently
/// points to Materialize admin endpoint.
pub static DEFAULT_ENDPOINT: LazyLock<Url> = LazyLock::new(|| {
    "https://admin.cloud.materialize.com"
        .parse()
        .expect("url known to be valid")
});

/// Configures the required parameters of a [`Client`].
pub struct ClientConfig {
    /// A singular, legitimate app password that will remain in use to identify
    /// the user throughout the client's existence.
    pub authentication: Authentication,
}

/// A builder for a [`Client`].
pub struct ClientBuilder {
    /// Endpoint to issue the requests from the client.
    endpoint: Url,
}

impl Default for ClientBuilder {
    /// The default option points to the Materialize admin endpoint.
    fn default() -> ClientBuilder {
        ClientBuilder {
            endpoint: DEFAULT_ENDPOINT.clone(),
        }
    }
}

impl ClientBuilder {
    /// Overrides the default endpoint.
    pub fn endpoint(mut self, endpoint: Url) -> ClientBuilder {
        self.endpoint = endpoint;
        self
    }

    /// Creates a [`Client`] that incorporates the optional parameters
    /// configured on the builder and the specified required parameters.
    pub fn build(self, config: ClientConfig) -> Client {
        let inner = reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap();
        Client {
            inner,
            authentication: config.authentication,
            endpoint: self.endpoint,
            auth: Default::default(),
        }
    }
}
