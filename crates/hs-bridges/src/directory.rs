//! The admin API's appservice directory with the bridge manager's knowledge added: a bridge
//! registered by hand for a network this server offers says so in its health
//! (`AppServiceHealth.overlaps_offering`, [`crate::overlap`]), and the registration of an
//! instance this server deploys carries its volume (`AppService.volume`, as the manager last
//! read it), so the Registrations page can say "volume full" rather than only "degraded".
//! Everything else is the inner directory's, untouched.

use std::sync::Arc;

use async_trait::async_trait;
use hs_admin::model::{
    AdminAppservice, AdminAppserviceCreate, AdminAppserviceHealth, AdminAppserviceReplay,
    AdminAppserviceTokens, AdminBridgeLogins,
};
use hs_admin::sources::{
    AdminAppserviceBacklogPage, AdminAppserviceRegistration, AppserviceDirectory,
    BacklogPageRequest, SourceError,
};
use hs_kv::KvBackend;
use serde_json::Value;

use crate::manager::BridgeManager;

/// [`AppserviceDirectory`] over `inner`, with each appservice's health saying which offering
/// it overlaps, when it does.
pub struct OfferingAwareDirectory<B: KvBackend> {
    inner: Arc<dyn AppserviceDirectory>,
    manager: Arc<BridgeManager<B>>,
}

impl<B: KvBackend> std::fmt::Debug for OfferingAwareDirectory<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfferingAwareDirectory")
            .finish_non_exhaustive()
    }
}

impl<B: KvBackend + 'static> OfferingAwareDirectory<B> {
    /// Over `inner`, asking `manager` about overlaps.
    pub fn new(inner: Arc<dyn AppserviceDirectory>, manager: Arc<BridgeManager<B>>) -> Self {
        Self { inner, manager }
    }

    async fn with_overlap(
        &self,
        id: &str,
        mut health: AdminAppserviceHealth,
    ) -> Result<AdminAppserviceHealth, SourceError> {
        health.overlaps_offering = self.manager.overlap_of_appservice(id).await?;
        Ok(health)
    }
}

#[async_trait]
impl<B: KvBackend + 'static> AppserviceDirectory for OfferingAwareDirectory<B> {
    async fn list(&self) -> Result<Vec<AdminAppservice>, SourceError> {
        let mut items = self.inner.list().await?;
        for item in &mut items {
            item.volume = self.manager.volume_of_appservice(&item.id);
        }
        Ok(items)
    }

    async fn get(&self, id: &str) -> Result<Option<AdminAppservice>, SourceError> {
        Ok(self.inner.get(id).await?.map(|mut item| {
            item.volume = self.manager.volume_of_appservice(&item.id);
            item
        }))
    }

    async fn create(&self, request: AdminAppserviceCreate) -> Result<AdminAppservice, SourceError> {
        self.inner.create(request).await
    }

    async fn update(&self, id: &str, patch: Value) -> Result<AdminAppservice, SourceError> {
        self.inner.update(id, patch).await
    }

    async fn delete(&self, id: &str) -> Result<(), SourceError> {
        self.inner.delete(id).await
    }

    async fn health(&self, id: &str) -> Result<AdminAppserviceHealth, SourceError> {
        let health = self.inner.health(id).await?;
        self.with_overlap(id, health).await
    }

    async fn backlog(
        &self,
        id: &str,
        page: BacklogPageRequest,
    ) -> Result<AdminAppserviceBacklogPage, SourceError> {
        self.inner.backlog(id, page).await
    }

    async fn pause(&self, id: &str) -> Result<AdminAppservice, SourceError> {
        self.inner.pause(id).await
    }

    async fn resume(&self, id: &str) -> Result<AdminAppservice, SourceError> {
        self.inner.resume(id).await
    }

    async fn rotate_tokens(&self, id: &str) -> Result<AdminAppserviceTokens, SourceError> {
        self.inner.rotate_tokens(id).await
    }

    async fn registration(&self, id: &str) -> Result<AdminAppserviceRegistration, SourceError> {
        self.inner.registration(id).await
    }

    async fn ping(&self, id: &str) -> Result<AdminAppserviceHealth, SourceError> {
        let health = self.inner.ping(id).await?;
        self.with_overlap(id, health).await
    }

    async fn replay(&self, id: &str, request: AdminAppserviceReplay) -> Result<usize, SourceError> {
        self.inner.replay(id, request).await
    }

    async fn discard(
        &self,
        id: &str,
        request: hs_admin::model::AdminAppserviceDiscard,
    ) -> Result<usize, SourceError> {
        self.inner.discard(id, request).await
    }

    async fn logins(
        &self,
        id: &str,
        user_id: Option<&str>,
    ) -> Result<AdminBridgeLogins, SourceError> {
        self.inner.logins(id, user_id).await
    }
}
