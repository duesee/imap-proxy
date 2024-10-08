use std::{net::SocketAddr, sync::Arc};

use colored::{Color, ColoredString, Colorize};
use imap_codec::fragmentizer::{FragmentInfo, Fragmentizer};
use imap_next::{
    client::{self, Client},
    imap_types::{
        command::{Command, CommandBody},
        extensions::idle::IdleDone,
        response::{Code, Greeting, Status},
        utils::escape_byte_string,
        ToStatic,
    },
    server::{self, Server},
    stream::{self, Stream},
    Interrupt, Io,
};
use once_cell::sync::Lazy;
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{
    rustls::{pki_types::ServerName, ClientConfig, RootCertStore, ServerConfig},
    TlsAcceptor, TlsConnector,
};
use tracing::{error, info, instrument, trace};

use crate::{
    config::{Bind, Connect, Identity, Service},
    util::{self, IdentityError},
};

static ROOT_CERT_STORE: Lazy<RootCertStore> = Lazy::new(|| {
    let mut root_store = RootCertStore::empty();

    for cert in rustls_native_certs::load_native_certs().unwrap() {
        root_store.add(cert).unwrap();
    }

    root_store
});

const LITERAL_ACCEPT_TEXT: &str = "proxy: Literal accepted by proxy";
const LITERAL_REJECT_TEXT: &str = "proxy: Literal rejected by proxy";
const COMMAND_REJECTED_TEXT: &str = "proxy: Command rejected by server";

const LAYER_TRANSPORT: &str = "transport";
const LAYER_FRAGMENT: &str = "fragment";
const LAYER_MESSAGE: &str = "message";

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error(transparent)]
    Tls(#[from] tokio_rustls::rustls::Error),
}

pub trait State: Send + 'static {}

pub struct Proxy<S: State> {
    service: Service,
    state: S,
}

pub struct BoundState {
    listener: TcpListener,
}

impl State for BoundState {}

impl Proxy<BoundState> {
    #[instrument(name = "event", fields(layer = LAYER_TRANSPORT), skip_all)]
    pub async fn bind(service: Service) -> Result<Self, ProxyError> {
        // Accept arbitrary number of connections.
        let bind_addr_port = service.bind.addr_port();
        let listener = TcpListener::bind(&bind_addr_port).await?;
        info!(?bind_addr_port, "Bound to");

        Ok(Self {
            service,
            state: BoundState { listener },
        })
    }

    #[instrument(name = "event", fields(layer = LAYER_TRANSPORT), skip_all)]
    pub async fn accept_client(&self) -> Result<Proxy<ClientAcceptedState>, ProxyError> {
        let (client_to_proxy, client_addr) = self.state.listener.accept().await?;
        info!(?client_addr, "Accepted client");

        let client_to_proxy = match &self.service.bind {
            Bind::Tls { identity, .. } => {
                let config = {
                    let (certificate_chain, leaf_key) = match identity {
                        Identity::CertificateChainAndLeafKey {
                            certificate_chain_path,
                            leaf_key_path,
                        } => {
                            let certificate_chain =
                                util::load_certificate_chain_pem(certificate_chain_path)?;
                            let leaf_key = util::load_leaf_key_pem(leaf_key_path)?;

                            (certificate_chain, leaf_key)
                        }
                    };

                    let mut config = ServerConfig::builder()
                        .with_no_client_auth()
                        // Note: The name is misleading. We provide the full chain here.
                        .with_single_cert(certificate_chain, leaf_key)?;

                    config.alpn_protocols = vec![b"imap".to_vec()];

                    config
                };

                // TODO(#146): The acceptor should really be part of the proxy initialization.
                //             However, for testing purposes, it's nice to create it on-the-fly.
                let acceptor = TlsAcceptor::from(Arc::new(config));

                info!(?client_addr, "Starting TLS with client");
                Stream::tls(acceptor.accept(client_to_proxy).await?.into())
            }
            Bind::Insecure { .. } => Stream::insecure(client_to_proxy),
        };

        Ok(Proxy {
            service: self.service.clone(),
            state: ClientAcceptedState {
                client_addr,
                client_to_proxy,
            },
        })
    }
}

pub struct ClientAcceptedState {
    client_addr: SocketAddr,
    client_to_proxy: Stream,
}

impl State for ClientAcceptedState {}

impl Proxy<ClientAcceptedState> {
    pub fn client_addr(&self) -> SocketAddr {
        self.state.client_addr
    }

    #[instrument(name = "event", fields(layer = LAYER_TRANSPORT), skip_all)]
    pub async fn connect_to_server(self) -> Result<Proxy<ConnectedState>, ProxyError> {
        let server_addr_port = self.service.connect.addr_port();
        info!(?server_addr_port, "Connecting to server");
        let stream_to_server = TcpStream::connect(&server_addr_port).await?;
        info!(?server_addr_port, "Connected to server");

        let proxy_to_server = match self.service.connect {
            Connect::Tls { ref host, .. } => {
                let config = {
                    let mut config = ClientConfig::builder()
                        .with_root_certificates(ROOT_CERT_STORE.clone())
                        .with_no_client_auth();

                    // See <https://www.iana.org/assignments/tls-extensiontype-values/tls-extensiontype-values.xhtml#alpn-protocol-ids>
                    config.alpn_protocols = vec![b"imap".to_vec()];

                    config
                };

                let connector = TlsConnector::from(Arc::new(config));
                let dnsname = ServerName::try_from(host.clone()).unwrap();

                info!(?server_addr_port, "Starting TLS with server");
                Stream::tls(connector.connect(dnsname, stream_to_server).await?.into())
            }
            Connect::Insecure { .. } => Stream::insecure(stream_to_server),
        };

        Ok(Proxy {
            service: self.service,
            state: ConnectedState {
                client_to_proxy: self.state.client_to_proxy,
                proxy_to_server,
            },
        })
    }
}

pub struct ConnectedState {
    client_to_proxy: Stream,
    proxy_to_server: Stream,
}

impl State for ConnectedState {}

impl Proxy<ConnectedState> {
    pub async fn start_conversation(self) {
        let mut proxy_to_server = {
            // TODO(#144): Read options from config
            let options = client::Options::default();
            let max_input_size = options.max_response_size;
            let client = Client::new(options);
            FragmentTracker::client(client, max_input_size)
        };
        let mut proxy_to_server_stream = self.state.proxy_to_server;
        let stream_event = proxy_to_server_stream.next(&mut proxy_to_server).await;
        let Some(server_event) = handle_stream_event("s2p", stream_event) else {
            return;
        };
        let Some(mut greeting) = handle_initial_server_event(server_event) else {
            return;
        };

        util::filter_capabilities_in_greeting(&mut greeting);

        let mut client_to_proxy = {
            // TODO(#144): Read options from config
            let mut options = server::Options::default();
            options
                .set_literal_accept_text(LITERAL_ACCEPT_TEXT.to_string())
                .unwrap();
            options
                .set_literal_reject_text(LITERAL_REJECT_TEXT.to_string())
                .unwrap();
            let max_input_size = options.max_command_size;
            let server = Server::new(options, greeting);
            FragmentTracker::server(server, max_input_size)
        };
        let mut client_to_proxy_stream = self.state.client_to_proxy;

        loop {
            tokio::select! {
                stream_event = client_to_proxy_stream.next(&mut client_to_proxy) => {
                    let Some(client_event) = handle_stream_event("c2p", stream_event) else {
                        break;
                    };
                    handle_client_event(client_event, &mut proxy_to_server)
                }
                stream_event = proxy_to_server_stream.next(&mut proxy_to_server) => {
                    let Some(server_event) = handle_stream_event("s2p", stream_event) else {
                        break;
                    };
                    handle_server_event(server_event, &mut client_to_proxy)
                }
            };
        }
    }
}

#[instrument(name = "event", fields(layer = LAYER_TRANSPORT), skip_all)]
fn handle_stream_event<T, E>(
    role: &'static str,
    stream_event: Result<T, stream::Error<E>>,
) -> Option<Result<T, E>> {
    match stream_event {
        Ok(event) => Some(Ok(event)),
        Err(stream::Error::Closed) => {
            info!(role, "Connection closed");
            None
        }
        Err(stream::Error::Io(error)) => {
            error!(role, %error, "Connection terminated");
            None
        }
        Err(stream::Error::Tls(error)) => {
            error!(role, %error, "Connection terminated");
            None
        }
        Err(stream::Error::State(error)) => Some(Err(error)),
    }
}

#[instrument(name = "event", fields(layer = LAYER_MESSAGE), skip_all)]
fn handle_initial_server_event(
    server_event: Result<client::Event, client::Error>,
) -> Option<Greeting<'static>> {
    match server_event {
        Ok(client::Event::GreetingReceived { greeting }) => {
            trace!(role = "s2p", greeting=%format!("{:?}", greeting).blue(), "<--|");
            Some(greeting)
        }
        Ok(event) => {
            error!(role = "s2p", ?event, "Unexpected event instead of greeting");
            None
        }
        Err(error) => {
            error!(role = "s2p", ?error, "Failed to receive greeting");
            None
        }
    }
}

#[instrument(name = "event", fields(layer = LAYER_MESSAGE), skip_all)]
fn handle_client_event(
    client_event: Result<server::Event, server::Error>,
    proxy_to_server: &mut FragmentTracker<Client>,
) {
    let event = match client_event {
        Ok(event) => event,
        Err(
            ref error @ (server::Error::ExpectedCrlfGotLf {
                ref discarded_bytes,
            }
            | server::Error::MalformedMessage {
                ref discarded_bytes,
            }
            | server::Error::LiteralTooLong {
                ref discarded_bytes,
            }
            | server::Error::CommandTooLong {
                ref discarded_bytes,
            }),
        ) => {
            error!(role = "c2p", %error, ?discarded_bytes, "Discard client message");
            return;
        }
    };

    match event {
        server::Event::GreetingSent { .. } => {
            trace!(role = "p2c", "<--- greeting");
        }
        server::Event::ResponseSent { handle, .. } => {
            trace!(role = "p2c", ?handle, "<---");
        }
        server::Event::CommandReceived { command } => {
            trace!(role = "c2p", command=%format!("{:?}", command).red(), "|-->");

            let handle = proxy_to_server.state.enqueue_command(command);
            trace!(role = "p2s", ?handle, "enqueue_command");
        }
        server::Event::CommandAuthenticateReceived {
            command_authenticate,
        } => {
            let command_authenticate: Command<'static> = command_authenticate.into();

            trace!(
                role = "c2p",
                command_authenticate=%format!("{:?}", command_authenticate).red(),
                "|-->"
            );

            let handle = proxy_to_server.state.enqueue_command(command_authenticate);
            trace!(role = "p2s", ?handle, "enqueue_command");
        }
        server::Event::AuthenticateDataReceived { authenticate_data } => {
            trace!(
                role = "c2p",
                authenticate_data=%format!("{:?}", authenticate_data).red(),
                "|-->"
            );

            // TODO(#145): Fix unwrap
            let handle = proxy_to_server
                .state
                .set_authenticate_data(authenticate_data)
                .unwrap();
            trace!(role = "p2s", ?handle, "set_authenticate_data");
        }
        server::Event::IdleCommandReceived { tag } => {
            let idle = Command {
                tag,
                body: CommandBody::Idle,
            };

            trace!(role = "c2p", idle=%format!("{:?}", idle).red(), "|-->");

            let handle = proxy_to_server.state.enqueue_command(idle);
            trace!(role = "p2s", ?handle, "enqueue_command");
        }
        server::Event::IdleDoneReceived => {
            trace!(role = "c2p", done=%format!("{:?}", IdleDone).red(), "|-->");

            let handle = proxy_to_server.state.set_idle_done();
            trace!(role = "p2s", ?handle, "set_idle_done");
        }
    }
}

#[instrument(name = "event", fields(layer = LAYER_MESSAGE), skip_all)]
fn handle_server_event(
    server_event: Result<client::Event, client::Error>,
    client_to_proxy: &mut FragmentTracker<Server>,
) {
    let event = match server_event {
        Ok(event) => event,
        Err(
            ref error @ (client::Error::ExpectedCrlfGotLf {
                ref discarded_bytes,
            }
            | client::Error::MalformedMessage {
                ref discarded_bytes,
            }
            | client::Error::ResponseTooLong {
                ref discarded_bytes,
            }),
        ) => {
            error!(role = "s2p", %error, ?discarded_bytes, "Discard server message");
            return;
        }
    };

    match event {
        client::Event::GreetingReceived { greeting } => {
            // This event is emitted only at the beginning so we must have already
            // handled it somewhere else.
            error!(role = "s2p", ?greeting, "Unexpected greeting");
        }
        client::Event::CommandSent { handle, .. } => {
            trace!(role = "p2s", ?handle, "--->");
        }
        client::Event::CommandRejected {
            handle,
            command,
            status,
        } => {
            trace!(role = "s2p", ?handle, status=%format!("{:?}", status).blue(), "<--|");

            let modified_status = match status.code() {
                Some(Code::Alert) => {
                    // Keep the alert message because it MUST be displayed to the user
                    Status::bad(
                        Some(command.tag),
                        Some(Code::Alert),
                        status.text().to_static(),
                    )
                    .unwrap()
                }
                _ => {
                    // Use generic message because the original code and text might be misleading
                    Status::bad(Some(command.tag), None, COMMAND_REJECTED_TEXT).unwrap()
                }
            };
            let handle = client_to_proxy
                .state
                .enqueue_status(modified_status.clone());
            trace!(
                role = "p2c",
                ?handle,
                modified_status=%format!("{:?}", modified_status).yellow(),
                "enqueue_status"
            );
        }
        client::Event::AuthenticateStarted { handle } => {
            trace!(role = "p2s", ?handle, "--->");
        }
        client::Event::AuthenticateContinuationRequestReceived {
            continuation_request,
            ..
        } => {
            trace!(
                role = "s2p",
                authenticate_continuation_request=%format!("{:?}", continuation_request).blue(),
                "<--|"
            );

            let handle = client_to_proxy
                .state
                .authenticate_continue(continuation_request)
                .unwrap();
            trace!(role = "p2c", ?handle, "authenticate_continue");
        }
        client::Event::AuthenticateStatusReceived { status, .. } => {
            trace!(role = "s2p", authenticate_status=%format!("{:?}", status).blue(), "<--|");

            // TODO(#145): Fix unwrap
            let handle = client_to_proxy.state.authenticate_finish(status).unwrap();
            trace!(role = "p2c", ?handle, "authenticate_finish");
        }
        client::Event::DataReceived { mut data } => {
            trace!(role = "s2p", data=%format!("{:?}", data).blue(), "<--|");

            util::filter_capabilities_in_data(&mut data);

            let handle = client_to_proxy.state.enqueue_data(data);
            trace!(role = "p2c", ?handle, "enqueue_data");
        }
        client::Event::StatusReceived { mut status } => {
            trace!(role = "s2p", status=%format!("{:?}", status).blue(), "<--|");

            util::filter_capabilities_in_status(&mut status);

            let handle = client_to_proxy.state.enqueue_status(status);
            trace!(role = "p2c", ?handle, "enqueue_status");
        }
        client::Event::ContinuationRequestReceived {
            mut continuation_request,
        } => {
            trace!(
                role = "s2p",
                continuation_request=%format!("{:?}", continuation_request).blue(),
                "<--|"
            );

            util::filter_capabilities_in_continuation(&mut continuation_request);

            let handle = client_to_proxy
                .state
                .enqueue_continuation_request(continuation_request);
            trace!(role = "p2c", ?handle, "enqueue_continuation_request");
        }
        client::Event::IdleCommandSent { handle } => {
            trace!(role = "p2s", ?handle, "--->");
        }
        client::Event::IdleAccepted {
            handle,
            continuation_request,
        } => {
            trace!(
                role = "s2p",
                ?handle,
                idle_accepted_continuation_request=%format!("{:?}", continuation_request).blue(),
                "<--|"
            );

            // TODO(#145): Fix unwrap
            let handle = client_to_proxy
                .state
                .idle_accept(continuation_request)
                .unwrap();
            trace!(role = "p2c", ?handle, "idle_accept");
        }
        client::Event::IdleRejected { handle, status } => {
            trace!(
                role = "s2p",
                ?handle,
                idle_rejected_status=%format!("{:?}", status).blue(),
                "<--|"
            );

            // TODO(#145): Fix unwrap
            let handle = client_to_proxy.state.idle_reject(status).unwrap();
            trace!(role = "p2c", ?handle, "idle_reject");
        }
        client::Event::IdleDoneSent { handle } => {
            trace!(role = "p2s", ?handle, "--->");
        }
    }
}

struct FragmentTracker<S> {
    input_role: &'static str,
    input_direction: &'static str,
    input_color: Color,
    input_fragmentizer: Fragmentizer,
    output_role: &'static str,
    output_direction: &'static str,
    output_fragmentizer: Fragmentizer,
    state: S,
}

impl FragmentTracker<Client> {
    pub fn client(state: Client, max_input_size: u32) -> Self {
        Self {
            input_role: "s2p",
            input_direction: "<--|",
            input_color: Color::Blue,
            input_fragmentizer: Fragmentizer::new(max_input_size),
            output_role: "p2s",
            output_direction: "--->",
            output_fragmentizer: Fragmentizer::without_max_message_size(),
            state,
        }
    }
}

impl FragmentTracker<Server> {
    pub fn server(state: Server, max_input_size: u32) -> Self {
        Self {
            input_role: "c2p",
            input_direction: "|-->",
            input_color: Color::Red,
            input_fragmentizer: Fragmentizer::new(max_input_size),
            output_role: "p2c",
            output_direction: "<---",
            output_fragmentizer: Fragmentizer::without_max_message_size(),
            state,
        }
    }
}

impl<S: imap_next::State> imap_next::State for FragmentTracker<S> {
    type Event = S::Event;
    type Error = S::Error;

    fn enqueue_input(&mut self, bytes: &[u8]) {
        self.input_fragmentizer.enqueue_bytes(bytes);
        while let Some(fragment_info) = self.input_fragmentizer.progress() {
            handle_fragment_event(
                self.input_role,
                self.input_direction,
                Some(self.input_color),
                &self.input_fragmentizer,
                fragment_info,
            );
        }

        self.state.enqueue_input(bytes);
    }

    fn next(&mut self) -> Result<Self::Event, Interrupt<Self::Error>> {
        let event = self.state.next();

        if let Err(Interrupt::Io(Io::Output(ref output))) = event {
            self.output_fragmentizer.enqueue_bytes(output);
            while let Some(fragment_info) = self.output_fragmentizer.progress() {
                handle_fragment_event(
                    self.output_role,
                    self.output_direction,
                    None,
                    &self.output_fragmentizer,
                    fragment_info,
                );
            }
        };

        event
    }
}

#[instrument(name = "event", fields(layer = LAYER_FRAGMENT), skip_all)]
fn handle_fragment_event(
    role: &'static str,
    direction: &'static str,
    color: Option<Color>,
    fragmentizer: &Fragmentizer,
    fragment_info: FragmentInfo,
) {
    let fragment_bytes = || escape_byte_string(fragmentizer.fragment_bytes(fragment_info));
    let exceeded = || fragmentizer.is_max_message_size_exceeded();

    match fragment_info {
        FragmentInfo::Line { .. } => {
            trace!(
                role,
                line=%maybe_color(format!("{:?}", fragment_bytes()), color),
                exceeded=%exceeded(),
                "{direction}"
            );
        }
        FragmentInfo::Literal { .. } => {
            trace!(
                role,
                literal=%maybe_color(format!("{:?}", fragment_bytes()), color),
                exceeded=%exceeded(),
                "{direction}"
            );
        }
    };
}

fn maybe_color(string: String, color: Option<Color>) -> ColoredString {
    match color {
        None => string.normal(),
        Some(c) => string.color(c),
    }
}
