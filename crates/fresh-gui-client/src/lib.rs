//! Host-side client stub for talking to `fresh-gui-backend`.

use fresh_gui_protocol::{Hello, PROTOCOL_VERSION};

/// Connection options (placeholder until transport is chosen).
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub endpoint: String,
}

impl ConnectOptions {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }
}

/// Client handle (no I/O yet).
#[derive(Debug)]
pub struct Client {
    opts: ConnectOptions,
    hello: Hello,
}

impl Client {
    pub fn prepare(opts: ConnectOptions) -> Self {
        let hello = Hello::client(
            format!("fresh-gui-client/{}", env!("CARGO_PKG_VERSION")),
            vec!["ping".into(), "pty".into()],
        );
        Self { opts, hello }
    }

    pub fn endpoint(&self) -> &str {
        &self.opts.endpoint
    }

    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    pub fn protocol_version() -> &'static str {
        PROTOCOL_VERSION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_sets_client_hello() {
        let client = Client::prepare(ConnectOptions::new("127.0.0.1:7420"));
        assert_eq!(client.endpoint(), "127.0.0.1:7420");
        assert_eq!(client.hello().protocol_version, PROTOCOL_VERSION);
    }
}
