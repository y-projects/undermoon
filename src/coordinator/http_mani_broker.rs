use super::broker::{MetaManipulationBroker, MetaManipulationBrokerError};
use crate::common::cluster::{Host, MigrationTaskMeta};
use crate::common::utils::ThreadSafe;
use std::pin::Pin;
use futures::{Future, TryFutureExt};
use reqwest;

#[derive(Clone)]
pub struct HttpMetaManipulationBroker {
    broker_address: String,
    client: reqwest::Client,
}

impl HttpMetaManipulationBroker {
    pub fn new(broker_address: String, client: reqwest::Client) -> Self {
        HttpMetaManipulationBroker {
            broker_address,
            client,
        }
    }
}

impl ThreadSafe for HttpMetaManipulationBroker {}

impl HttpMetaManipulationBroker {
    async fn replace_proxy_impl(
        &self,
        failed_proxy_address: String,
    ) -> Result<Host, MetaManipulationBrokerError> {
        let url = format!(
            "http://{}/api/proxies/failover/{}",
            self.broker_address, failed_proxy_address
        );
        let response = self.client.post(&url).send().await.map_err(|e| {
            error!("Failed to replace proxy {:?}", e);
            MetaManipulationBrokerError::InvalidReply
        })?;


        let status = response.status();

        if status.is_success() {
            response.json().await.map_err(|e| {
                error!("Failed to get json payload {:?}", e);
                MetaManipulationBrokerError::InvalidReply
            })
        } else {
            error!(
                "replace_proxy: Failed to replace node: status code {:?}",
                status
            );
            let result = response.text().await;
            match result {
                Ok(body) => {
                    error!("replace_proxy: Error body: {:?}", body);
                    Err(MetaManipulationBrokerError::InvalidReply)
                }
                Err(e) => {
                    error!("replace_proxy: Failed to get body: {:?}", e);
                    Err(MetaManipulationBrokerError::InvalidReply)
                }
            }
        }
    }

    async fn commit_migration_impl(
        &self,
        meta: MigrationTaskMeta,
    ) -> Result<(), MetaManipulationBrokerError> {
        let url = format!("http://{}/api/clusters/migrations", self.broker_address);

        let response = self.client.put(&url).json(&meta).send().await.map_err(|e| {
            error!("Failed to commit migration {:?}", e);
            MetaManipulationBrokerError::InvalidReply
        })?;

        let status = response.status();

        if status.is_success() || status.as_u16() == 404 {
            Ok(())
        } else {
            error!("Failed to commit migration status code {:?}", status);
            let result = response.text().await;
            match result {
                Ok(body) => {
                    error!("HttpMetaManipulationBroker::commit_migration Error body: {:?}", body);
                    Err(MetaManipulationBrokerError::InvalidReply)
                }
                Err(e) => {
                    error!("HttpMetaManipulationBroker::commit_migration Failed to get body: {:?}", e);
                    Err(MetaManipulationBrokerError::InvalidReply)
                }
            }
        }
    }
}

impl MetaManipulationBroker for HttpMetaManipulationBroker {
    fn replace_proxy(
        &self,
        failed_proxy_address: String,
    ) -> Pin<Box<dyn Future<Output = Result<Host, MetaManipulationBrokerError>> + Send>> {
        Box::pin(self.replace_proxy_impl(failed_proxy_address))
    }

    fn commit_migration(
        &self,
        meta: MigrationTaskMeta,
    ) -> Pin<Box<dyn Future<Output = Result<(), MetaManipulationBrokerError>> + Send>> {
        Box::pin(self.commit_migration_impl(meta))
    }
}
