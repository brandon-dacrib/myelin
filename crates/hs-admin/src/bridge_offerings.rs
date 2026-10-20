//! The data source behind the `bridge_deployments.*`, `bridge_offerings.*` and
//! `bridge_instances.*` operations (RFC 0017): bridge types switched on for this server, and the
//! per-user instances of each. Implemented for real by `hs-bridges`' manager; by
//! [`InMemoryBridgeOfferings`] for this crate's tests and `hs-admin-mock`.

use std::collections::BTreeMap;
use std::sync::RwLock;

use async_trait::async_trait;

use crate::model::{
    BridgeBotRoom, BridgeConnection, BridgeConnectionCreate, BridgeConnectionCreated,
    BridgeConnectionList, BridgeDeploymentTarget, BridgeInstance, BridgeInstanceFiles,
    BridgeOffering, BridgeOfferingRequest, BridgeWebhookSource,
};
use crate::sources::SourceError;

/// The user id a shared type's one instance is addressed by in paths.
pub const SHARED_INSTANCE: &str = "_";

/// Offerings and their instances. Every method naming a type that is not offered answers
/// [`SourceError::NotFound`], except [`BridgeOfferingSource::put`], which creates it (and
/// answers [`SourceError::NotFound`] only for a type the catalogue does not have).
#[async_trait]
pub trait BridgeOfferingSource: Send + Sync + 'static {
    /// Whether this server can deploy instances itself, and where.
    async fn target(&self) -> BridgeDeploymentTarget;
    async fn list(&self) -> Result<Vec<BridgeOffering>, SourceError>;
    async fn get(&self, bridge_type: &str) -> Result<Option<BridgeOffering>, SourceError>;
    /// Creates or changes an offering, registering its front door.
    async fn put(
        &self,
        bridge_type: &str,
        request: BridgeOfferingRequest,
    ) -> Result<BridgeOffering, SourceError>;
    /// [`SourceError::Conflict`] while instances remain, unless `remove_instances`.
    async fn delete(&self, bridge_type: &str, remove_instances: bool) -> Result<(), SourceError>;
    async fn instances(&self, bridge_type: &str) -> Result<Vec<BridgeInstance>, SourceError>;
    async fn instance(
        &self,
        bridge_type: &str,
        user_id: &str,
    ) -> Result<Option<BridgeInstance>, SourceError>;
    /// Starts provisioning an instance for `user_id`; idempotent, and retries a failed one.
    async fn put_instance(
        &self,
        bridge_type: &str,
        user_id: &str,
    ) -> Result<BridgeInstance, SourceError>;
    /// Tells this server where an instance run elsewhere listens (`host`, `host:port` or a
    /// URL; a bare host takes the type's port): its registration's `url` and its files follow.
    /// [`SourceError::Invalid`] for an instance this server deploys.
    async fn set_instance_address(
        &self,
        bridge_type: &str,
        user_id: &str,
        address: &str,
    ) -> Result<BridgeInstance, SourceError>;
    /// Sets the size of the volume of an instance this server deploys
    /// ([`crate::bridge_volume::parse_size`]: a whole `Mi`, `Gi` or `Ti`, 1Gi to 1Ti, never
    /// less than the volume has now). The volume grows in place and the bridge restarts.
    /// [`SourceError::InvalidField`] at `/volume_size` for a size refused, or for an instance run
    /// elsewhere.
    async fn set_instance_volume_size(
        &self,
        bridge_type: &str,
        user_id: &str,
        size: &str,
    ) -> Result<BridgeInstance, SourceError>;
    async fn delete_instance(&self, bridge_type: &str, user_id: &str) -> Result<(), SourceError>;
    /// The files to run an instance elsewhere, with its own tokens.
    async fn instance_files(
        &self,
        bridge_type: &str,
        user_id: &str,
    ) -> Result<BridgeInstanceFiles, SourceError>;
    /// What a shared integrations bridge (Hookshot) has connected, room by room: read from the
    /// rooms its bot is in, with those rooms. Empty for a type that keeps no connections in
    /// room state, or whose instance is not running yet.
    async fn connections(&self, bridge_type: &str) -> Result<BridgeConnectionList, SourceError>;
    /// The webhook sources a connection can be made from. Empty for a type that takes none.
    async fn webhook_sources(
        &self,
        bridge_type: &str,
    ) -> Result<Vec<BridgeWebhookSource>, SourceError>;
    /// Makes a webhook connection as the bridge's bot: the room if asked, the hook id, the
    /// bot's account data, the state event. [`SourceError::InvalidField`] for a bad source,
    /// name or room; [`SourceError::Conflict`] for a name the room already has;
    /// [`SourceError::Invalid`] when the bridge is not running yet or the bot may not write
    /// state in the room.
    async fn create_connection(
        &self,
        bridge_type: &str,
        request: BridgeConnectionCreate,
    ) -> Result<BridgeConnectionCreated, SourceError>;
    /// Removes the connection whose state key is `state_key` in `room_id` the way the bridge
    /// does: its state event emptied, and for a webhook its hook id dropped from the bot's
    /// account data. [`SourceError::NotFound`] when there is no such active connection.
    async fn delete_connection(
        &self,
        bridge_type: &str,
        room_id: &str,
        state_key: &str,
    ) -> Result<(), SourceError>;
}

/// A [`BridgeOfferingSource`] held in memory: offerings are stored as given, instances are
/// created `requested` and stay there. For tests and the mock server.
#[derive(Debug, Default)]
pub struct InMemoryBridgeOfferings {
    target: BridgeDeploymentTarget,
    offerings: RwLock<BTreeMap<String, BridgeOffering>>,
    instances: RwLock<BTreeMap<(String, String), BridgeInstance>>,
    /// Webhook connections made through [`BridgeOfferingSource::create_connection`], by type.
    connections: RwLock<Vec<(String, BridgeConnection)>>,
}

impl InMemoryBridgeOfferings {
    /// With deployment available into `namespace`.
    #[must_use]
    pub fn in_cluster(namespace: &str) -> Self {
        Self {
            target: BridgeDeploymentTarget {
                available: true,
                namespace: Some(namespace.to_owned()),
                homeserver_url: Some(format!("http://myelin.{namespace}.svc:8008")),
                reason: None,
            },
            ..Self::default()
        }
    }

    /// Two sources, enough for the router's tests and the interface's mocks: the plain one and
    /// one with a transformation function and a file.
    fn sources() -> Vec<BridgeWebhookSource> {
        vec![
            BridgeWebhookSource {
                id: "generic".into(),
                title: "Plain JSON".into(),
                description: "Anything that posts JSON.".into(),
                setup: "Post JSON to `{{url}}`.".into(),
                transforms: false,
                files: Vec::new(),
            },
            BridgeWebhookSource {
                id: "gmail".into(),
                title: "Gmail".into(),
                description: "New threads under one label, through an Apps Script.".into(),
                setup: "Paste Code.gs; it posts to `{{url}}`.".into(),
                transforms: true,
                files: vec!["Code.gs".into()],
            },
        ]
    }

    fn counts(&self, bridge_type: &str) -> BTreeMap<String, u64> {
        let mut out = BTreeMap::new();
        for ((t, _), i) in self.instances.read().expect("lock").iter() {
            if t == bridge_type {
                *out.entry(i.state.clone()).or_insert(0) += 1;
            }
        }
        out
    }

    fn with_counts(&self, mut offering: BridgeOffering) -> BridgeOffering {
        offering.instances = self.counts(&offering.bridge_type);
        offering
    }
}

#[async_trait]
impl BridgeOfferingSource for InMemoryBridgeOfferings {
    async fn target(&self) -> BridgeDeploymentTarget {
        self.target.clone()
    }

    async fn list(&self) -> Result<Vec<BridgeOffering>, SourceError> {
        let offerings: Vec<_> = self
            .offerings
            .read()
            .expect("lock")
            .values()
            .cloned()
            .collect();
        Ok(offerings.into_iter().map(|o| self.with_counts(o)).collect())
    }

    async fn get(&self, bridge_type: &str) -> Result<Option<BridgeOffering>, SourceError> {
        let offering = self
            .offerings
            .read()
            .expect("lock")
            .get(bridge_type)
            .cloned();
        Ok(offering.map(|o| self.with_counts(o)))
    }

    async fn put(
        &self,
        bridge_type: &str,
        request: BridgeOfferingRequest,
    ) -> Result<BridgeOffering, SourceError> {
        let Some(kind) = crate::bridge_types::get(bridge_type, "example.org") else {
            return Err(SourceError::NotFound);
        };
        let runtime = request.runtime.clone().unwrap_or_else(|| "cluster".into());
        if runtime == "cluster" && !self.target.available {
            return Err(SourceError::InvalidField {
                pointer: "/runtime",
                detail: "this server cannot deploy bridges".into(),
            });
        }
        let mut offerings = self.offerings.write().expect("lock");
        let current = offerings.get(bridge_type).cloned();
        // The tag asked for, else the one kept, else (and for `latest`) the catalogue's pin.
        let image_tag = crate::bridge_types::image_tag(
            bridge_type,
            request
                .image_tag
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .or(current.as_ref().map(|c| c.image_tag.as_str())),
        )
        .unwrap_or_else(|| "latest".to_owned());
        let repository = kind
            .image
            .rsplit_once(':')
            .map_or(kind.image.as_str(), |(r, _)| r);
        let offering = BridgeOffering {
            bridge_type: bridge_type.to_owned(),
            name: kind.name.clone(),
            mode: kind.mode.clone(),
            enabled: request
                .enabled
                .or(current.as_ref().map(|c| c.enabled))
                .unwrap_or(true),
            runtime,
            image_tag: image_tag.clone(),
            image: format!("{repository}:{image_tag}"),
            front_door: (kind.mode == "per_user").then(|| {
                kind.default_namespaces["users"][1]["regex"]
                    .as_str()
                    .unwrap_or_default()
                    .replace('\\', "")
            }),
            access: request
                .access
                .or(current.as_ref().map(|c| c.access.clone()))
                .unwrap_or_default(),
            options: request
                .options
                .or(current.as_ref().map(|c| c.options.clone()))
                .unwrap_or_default(),
            instances: BTreeMap::new(),
            created_at: current
                .map(|c| c.created_at)
                .unwrap_or_else(|| "2026-09-26T00:00:00.000Z".into()),
            overlapping_appservices: Vec::new(),
        };
        offerings.insert(bridge_type.to_owned(), offering.clone());
        drop(offerings);
        Ok(self.with_counts(offering))
    }

    async fn delete(&self, bridge_type: &str, remove_instances: bool) -> Result<(), SourceError> {
        if self.get(bridge_type).await?.is_none() {
            return Err(SourceError::NotFound);
        }
        let mut instances = self.instances.write().expect("lock");
        let has = instances.keys().any(|(t, _)| t == bridge_type);
        if has && !remove_instances {
            return Err(SourceError::Conflict(format!(
                "{bridge_type} still has instances"
            )));
        }
        instances.retain(|(t, _), _| t != bridge_type);
        drop(instances);
        self.offerings.write().expect("lock").remove(bridge_type);
        Ok(())
    }

    async fn instances(&self, bridge_type: &str) -> Result<Vec<BridgeInstance>, SourceError> {
        if self.get(bridge_type).await?.is_none() {
            return Err(SourceError::NotFound);
        }
        Ok(self
            .instances
            .read()
            .expect("lock")
            .iter()
            .filter(|((t, _), _)| t == bridge_type)
            .map(|(_, i)| i.clone())
            .collect())
    }

    async fn instance(
        &self,
        bridge_type: &str,
        user_id: &str,
    ) -> Result<Option<BridgeInstance>, SourceError> {
        Ok(self
            .instances
            .read()
            .expect("lock")
            .get(&(bridge_type.to_owned(), user_id.to_owned()))
            .cloned())
    }

    async fn put_instance(
        &self,
        bridge_type: &str,
        user_id: &str,
    ) -> Result<BridgeInstance, SourceError> {
        let Some(offering) = self.get(bridge_type).await? else {
            return Err(SourceError::NotFound);
        };
        let offering_runtime = offering.runtime;
        let mut instances = self.instances.write().expect("lock");
        let instance = instances
            .entry((bridge_type.to_owned(), user_id.to_owned()))
            .or_insert_with(|| BridgeInstance {
                bridge_type: bridge_type.to_owned(),
                user_id: (user_id != SHARED_INSTANCE).then(|| user_id.to_owned()),
                state: "requested".into(),
                created_at: "2026-09-26T00:00:00.000Z".into(),
                volume: (offering_runtime == "cluster").then(|| {
                    crate::bridge_volume::view(crate::bridge_volume::DEFAULT_SIZE, false, None)
                }),
                ..BridgeInstance::default()
            });
        Ok(instance.clone())
    }

    async fn set_instance_address(
        &self,
        bridge_type: &str,
        user_id: &str,
        address: &str,
    ) -> Result<BridgeInstance, SourceError> {
        let offering = self.get(bridge_type).await?.ok_or(SourceError::NotFound)?;
        if offering.runtime == "cluster" {
            return Err(SourceError::Invalid(
                "this server deploys that bridge itself and knows where it listens".into(),
            ));
        }
        let port = crate::bridge_types::get(bridge_type, "example.org")
            .map(|k| k.port)
            .unwrap_or(29318);
        let url = instance_url(address, port).map_err(SourceError::Invalid)?;
        let mut instances = self.instances.write().expect("lock");
        let instance = instances
            .get_mut(&(bridge_type.to_owned(), user_id.to_owned()))
            .ok_or(SourceError::NotFound)?;
        instance.url = Some(url);
        Ok(instance.clone())
    }

    async fn set_instance_volume_size(
        &self,
        bridge_type: &str,
        user_id: &str,
        size: &str,
    ) -> Result<BridgeInstance, SourceError> {
        let offering = self.get(bridge_type).await?.ok_or(SourceError::NotFound)?;
        if offering.runtime != "cluster" {
            return Err(SourceError::InvalidField {
                pointer: "/volume_size",
                detail: "this server does not deploy that bridge: its volume is wherever it runs"
                    .into(),
            });
        }
        let mut instances = self.instances.write().expect("lock");
        let instance = instances
            .get_mut(&(bridge_type.to_owned(), user_id.to_owned()))
            .ok_or(SourceError::NotFound)?;
        let current = instance
            .volume
            .as_ref()
            .map(|v| v.cluster_size.clone().unwrap_or_else(|| v.size.clone()));
        let (size, _) =
            crate::bridge_volume::parse_size(size, current.as_deref()).map_err(|detail| {
                SourceError::InvalidField {
                    pointer: "/volume_size",
                    detail,
                }
            })?;
        let deployment = instance.deployment.clone();
        instance.volume = Some(crate::bridge_volume::view(&size, true, deployment.as_ref()));
        Ok(instance.clone())
    }

    async fn delete_instance(&self, bridge_type: &str, user_id: &str) -> Result<(), SourceError> {
        self.instances
            .write()
            .expect("lock")
            .remove(&(bridge_type.to_owned(), user_id.to_owned()))
            .map(|_| ())
            .ok_or(SourceError::NotFound)
    }

    async fn instance_files(
        &self,
        bridge_type: &str,
        user_id: &str,
    ) -> Result<BridgeInstanceFiles, SourceError> {
        if self.instance(bridge_type, user_id).await?.is_none() {
            return Err(SourceError::NotFound);
        }
        let config = "homeserver:\n  domain: example.org\n".to_owned();
        let registration = format!("id: {bridge_type}\n");
        Ok(BridgeInstanceFiles {
            config_yaml: Some(config.clone()),
            registration_yaml: registration.clone(),
            compose_yaml: "services: {}\n".into(),
            manifest_yaml: "kind: Bridge\n".into(),
            run_sh: (bridge_type == "mautrix-imessage").then(|| "#!/bin/sh\n".into()),
            files: BTreeMap::from([
                ("config.yaml".to_owned(), config),
                ("registration.yaml".to_owned(), registration),
            ]),
        })
    }

    async fn connections(&self, bridge_type: &str) -> Result<BridgeConnectionList, SourceError> {
        if self.get(bridge_type).await?.is_none() {
            return Err(SourceError::NotFound);
        }
        let items: Vec<BridgeConnection> = self
            .connections
            .read()
            .expect("lock")
            .iter()
            .filter(|(t, _)| t == bridge_type)
            .map(|(_, c)| c.clone())
            .collect();
        let mut rooms: Vec<BridgeBotRoom> = Vec::new();
        for c in &items {
            if !rooms.iter().any(|r| r.room_id == c.room_id) {
                rooms.push(BridgeBotRoom {
                    room_id: c.room_id.clone(),
                    room_name: c.room_name.clone(),
                });
            }
        }
        Ok(BridgeConnectionList { items, rooms })
    }

    async fn webhook_sources(
        &self,
        bridge_type: &str,
    ) -> Result<Vec<BridgeWebhookSource>, SourceError> {
        if self.get(bridge_type).await?.is_none() {
            return Err(SourceError::NotFound);
        }
        if !crate::bridge_types::keeps_connections_in_room_state(bridge_type) {
            return Ok(Vec::new());
        }
        Ok(Self::sources())
    }

    async fn create_connection(
        &self,
        bridge_type: &str,
        request: BridgeConnectionCreate,
    ) -> Result<BridgeConnectionCreated, SourceError> {
        let offering = self.get(bridge_type).await?.ok_or(SourceError::NotFound)?;
        let source = Self::sources()
            .into_iter()
            .find(|s| s.id == request.source)
            .ok_or_else(|| SourceError::InvalidField {
                pointer: "/source",
                detail: format!("`{}` is not a webhook source", request.source),
            })?;
        validate_connection_name(&request.name)?;
        let room_id = match (&request.room.id, &request.room.create) {
            (Some(id), None) => id.clone(),
            (None, Some(create)) => {
                if create.name.trim().is_empty() {
                    return Err(SourceError::InvalidField {
                        pointer: "/room/create/name",
                        detail: "the room needs a name".into(),
                    });
                }
                format!("!{}:example.org", crate::model::new_id())
            }
            _ => {
                return Err(SourceError::InvalidField {
                    pointer: "/room",
                    detail: "say which room: `id` of one the bot is in, or `create` one".into(),
                });
            }
        };
        let mut connections = self.connections.write().expect("lock");
        if connections.iter().any(|(t, c)| {
            t == bridge_type
                && c.room_id == room_id
                && c.state_key == request.name
                && c.state == "active"
        }) {
            return Err(SourceError::Conflict(format!(
                "the room already has a connection named `{}`",
                request.name
            )));
        }
        let public_url = offering
            .options
            .public_url
            .clone()
            .unwrap_or_else(|| "https://hooks.example.org".into());
        let url = format!(
            "{}/webhook/{}",
            public_url.trim_end_matches('/'),
            crate::model::new_id()
        );
        connections.push((
            bridge_type.to_owned(),
            BridgeConnection {
                service: "webhook".into(),
                event_type: "uk.half-shot.matrix-hookshot.generic.hook".into(),
                state_key: request.name.clone(),
                room_id: room_id.clone(),
                room_name: request.room.create.as_ref().map(|c| c.name.clone()),
                name: request.name.clone(),
                state: "active".into(),
                source: Some(source.id.clone()),
                content: serde_json::json!({"name": request.name, "io.myelin.source": source.id}),
            },
        ));
        Ok(BridgeConnectionCreated {
            room_id,
            name: request.name,
            setup: source.setup.replace("{{url}}", &url),
            files: source
                .files
                .iter()
                .map(|f| (f.clone(), format!("// {f} for {url}\n")))
                .collect(),
            source: source.id,
            url,
        })
    }

    async fn delete_connection(
        &self,
        bridge_type: &str,
        room_id: &str,
        state_key: &str,
    ) -> Result<(), SourceError> {
        if self.get(bridge_type).await?.is_none() {
            return Err(SourceError::NotFound);
        }
        let mut connections = self.connections.write().expect("lock");
        let found = connections.iter_mut().find(|(t, c)| {
            t == bridge_type
                && c.room_id == room_id
                && c.state_key == state_key
                && c.state == "active"
        });
        match found {
            Some((_, c)) => {
                c.state = "removed".into();
                c.content = serde_json::json!({});
                c.source = None;
                Ok(())
            }
            None => Err(SourceError::NotFound),
        }
    }
}

/// Hookshot's rule for a connection's name (`GenericHookConnection.validateState`): a string
/// of 3 to 64 characters.
///
/// # Errors
/// [`SourceError::InvalidField`] at `/name`.
pub fn validate_connection_name(name: &str) -> Result<(), SourceError> {
    let len = name.chars().count();
    if name.trim() != name || !(3..=64).contains(&len) {
        return Err(SourceError::InvalidField {
            pointer: "/name",
            detail: "the name is 3 to 64 characters, with no space at either end".into(),
        });
    }
    Ok(())
}

/// The registration `url` an administrator's `address` means: a full `http(s)://` URL as given
/// (its trailing slash dropped), else `host` or `host:port` over plain HTTP, a bare host taking
/// `default_port`. Refused when empty, when it has a path or query, or when the host is not a
/// hostname, an IPv4 address or a bracketed IPv6 address.
///
/// # Errors
/// What is wrong with it, in words for the administrator.
pub fn instance_url(address: &str, default_port: u16) -> Result<String, String> {
    let address = address.trim();
    if address.is_empty() {
        return Err(
            "address is empty: the Mac's hostname or IP address, with :port if not the default"
                .into(),
        );
    }
    let (scheme, rest) = match address.split_once("://") {
        Some(("http", rest)) => ("http", rest),
        Some(("https", rest)) => ("https", rest),
        Some((other, _)) => {
            return Err(format!("address: `{other}://` is not http or https"));
        }
        None => ("http", address),
    };
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() || rest.contains('/') || rest.contains('?') || rest.contains('#') {
        return Err("address is a host or host:port (or an http(s):// URL with no path)".into());
    }
    let (host, port) = if let Some(end) = rest.strip_prefix('[') {
        let Some((v6, after)) = end.split_once(']') else {
            return Err("address: an IPv6 address is written in brackets, [::1]:29331".into());
        };
        if v6.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(format!("address: `{v6}` is not an IPv6 address"));
        }
        let port = match after.strip_prefix(':') {
            None if after.is_empty() => default_port,
            Some(p) => parse_port(p)?,
            None => return Err("address: after ] comes :port or nothing".into()),
        };
        (format!("[{v6}]"), port)
    } else {
        match rest.rsplit_once(':') {
            Some((host, p)) if !host.contains(':') => (host.to_owned(), parse_port(p)?),
            _ => (rest.to_owned(), default_port),
        }
    };
    let host_ok = host.starts_with('[')
        || (!host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            }));
    if !host_ok {
        return Err(format!(
            "address: `{host}` is not a hostname or an IP address"
        ));
    }
    Ok(format!("{scheme}://{host}:{port}"))
}

fn parse_port(p: &str) -> Result<u16, String> {
    p.parse::<u16>()
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| format!("address: `{p}` is not a port"))
}

#[cfg(test)]
mod address_tests {
    use super::instance_url;

    #[test]
    fn an_address_becomes_the_registration_url() {
        assert_eq!(
            instance_url("brandons-mac.local", 29331).unwrap(),
            "http://brandons-mac.local:29331"
        );
        assert_eq!(
            instance_url(" 192.168.1.20:29400 ", 29331).unwrap(),
            "http://192.168.1.20:29400"
        );
        assert_eq!(
            instance_url("https://mac.example.net/", 29331).unwrap(),
            "https://mac.example.net:29331"
        );
        assert_eq!(
            instance_url("http://mac.example.net:8080", 29331).unwrap(),
            "http://mac.example.net:8080"
        );
        assert_eq!(
            instance_url("[fd00::2]", 29331).unwrap(),
            "http://[fd00::2]:29331"
        );
        assert_eq!(
            instance_url("[fd00::2]:29400", 29331).unwrap(),
            "http://[fd00::2]:29400"
        );
        for bad in [
            "",
            "   ",
            "ws://mac",
            "mac/path",
            "mac:notaport",
            "mac:0",
            "mac:70000",
            "[fd00::2",
            "[nope]",
            "a b",
            "mac?x=1",
        ] {
            assert!(instance_url(bad, 29331).is_err(), "{bad:?}");
        }
    }
}

#[cfg(test)]
mod volume_tests {
    use super::*;

    #[tokio::test]
    async fn a_deployed_instances_volume_size_is_set_and_never_shrunk() {
        let source = InMemoryBridgeOfferings::in_cluster("myelin");
        source
            .put("mautrix-slack", BridgeOfferingRequest::default())
            .await
            .unwrap();
        let created = source
            .put_instance("mautrix-slack", "@alice:example.org")
            .await
            .unwrap();
        let volume = created.volume.unwrap();
        assert_eq!(volume.size, crate::bridge_volume::DEFAULT_SIZE);
        assert!(!volume.size_set);

        let raised = source
            .set_instance_volume_size("mautrix-slack", "@alice:example.org", "20Gi")
            .await
            .unwrap();
        let volume = raised.volume.unwrap();
        assert_eq!(volume.size, "20Gi");
        assert!(volume.size_set);

        let shrink = source
            .set_instance_volume_size("mautrix-slack", "@alice:example.org", "10Gi")
            .await
            .unwrap_err();
        assert!(
            matches!(&shrink, SourceError::InvalidField { pointer: "/volume_size", detail } if detail.contains("cannot shrink")),
            "{shrink:?}"
        );
        let junk = source
            .set_instance_volume_size("mautrix-slack", "@alice:example.org", "lots")
            .await
            .unwrap_err();
        assert!(matches!(junk, SourceError::InvalidField { .. }), "{junk:?}");
        assert!(matches!(
            source
                .set_instance_volume_size("mautrix-slack", "@bob:example.org", "20Gi")
                .await,
            Err(SourceError::NotFound)
        ));
    }

    #[tokio::test]
    async fn an_instance_run_elsewhere_has_no_volume_to_size() {
        let source = InMemoryBridgeOfferings::default();
        source
            .put(
                "mautrix-imessage",
                BridgeOfferingRequest {
                    runtime: Some("elsewhere".into()),
                    ..BridgeOfferingRequest::default()
                },
            )
            .await
            .unwrap();
        let created = source
            .put_instance("mautrix-imessage", "@alice:example.org")
            .await
            .unwrap();
        assert!(created.volume.is_none());
        let refused = source
            .set_instance_volume_size("mautrix-imessage", "@alice:example.org", "10Gi")
            .await
            .unwrap_err();
        assert!(
            matches!(
                refused,
                SourceError::InvalidField {
                    pointer: "/volume_size",
                    ..
                }
            ),
            "{refused:?}"
        );
    }
}
