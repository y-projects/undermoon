use super::backend::{
    CachedSenderFactory, CmdTask, DirectionSenderFactory, RRSenderGroupFactory,
    RecoverableBackendNodeFactory,
};
use super::command::CmdType;
use super::database::{DBError, DBSendError, DBTag, DatabaseMap};
use super::session::{CmdCtx, CmdCtxHandler};
use ::migration::manager::MigrationManager;
use ::migration::task::MigrationConfig;
use caseless;
use common::db::HostDBMap;
use common::utils::{ThreadSafe, OLD_EPOCH_REPLY};
use protocol::{Array, BulkStr, RedisClientFactory, Resp};
use replication::manager::ReplicatorManager;
use replication::replicator::ReplicatorMeta;
use std::str;
use std::sync::{self, Arc};

pub struct SharedForwardHandler<F: RedisClientFactory> {
    handler: sync::Arc<ForwardHandler<F>>,
}

impl<F: RedisClientFactory> ThreadSafe for SharedForwardHandler<F> {}

impl<F: RedisClientFactory> Clone for SharedForwardHandler<F> {
    fn clone(&self) -> Self {
        Self {
            handler: self.handler.clone(),
        }
    }
}

impl<F: RedisClientFactory> SharedForwardHandler<F> {
    pub fn new(service_address: String, client_factory: Arc<F>) -> Self {
        Self {
            handler: sync::Arc::new(ForwardHandler::new(service_address, client_factory)),
        }
    }
}

impl<F: RedisClientFactory> CmdCtxHandler for SharedForwardHandler<F> {
    fn handle_cmd_ctx(&self, cmd_ctx: CmdCtx) {
        self.handler.handle_cmd_ctx(cmd_ctx)
    }
}

pub struct ForwardHandler<F: RedisClientFactory> {
    service_address: String,
    db: DatabaseMap<
        CachedSenderFactory<RRSenderGroupFactory<RecoverableBackendNodeFactory<CmdCtx>>>,
    >,
    replicator_manager: ReplicatorManager<F>,
    migration_manager: MigrationManager<F, DirectionSenderFactory<CmdCtx>>,
}

impl<F: RedisClientFactory> ForwardHandler<F> {
    pub fn new(service_address: String, client_factory: Arc<F>) -> Self {
        let sender_factory = CachedSenderFactory::new(RRSenderGroupFactory::new(
            RecoverableBackendNodeFactory::default(),
        ));
        let db = DatabaseMap::new(sender_factory);
        let redirection_sender_factory = Arc::new(DirectionSenderFactory::default());
        let migration_config = Arc::new(MigrationConfig::default());
        Self {
            service_address,
            db,
            replicator_manager: ReplicatorManager::new(client_factory.clone()),
            migration_manager: MigrationManager::new(
                migration_config,
                client_factory,
                redirection_sender_factory,
            ),
        }
    }
}

impl<F: RedisClientFactory> ForwardHandler<F> {
    fn handle_auth(&self, cmd_ctx: CmdCtx) {
        let key = cmd_ctx.get_cmd().get_key();
        match key {
            None => cmd_ctx.set_resp_result(Ok(Resp::Error(
                String::from("Missing database name").into_bytes(),
            ))),
            Some(db_name) => match str::from_utf8(&db_name) {
                Ok(ref db) => {
                    cmd_ctx.set_db_name(db.to_string());
                    cmd_ctx.set_resp_result(Ok(Resp::Simple(String::from("OK").into_bytes())))
                }
                Err(_) => cmd_ctx.set_resp_result(Ok(Resp::Error(
                    String::from("Invalid database name").into_bytes(),
                ))),
            },
        }
    }

    fn handle_cluster(&self, cmd_ctx: CmdCtx) {
        let (cmd_ctx, sub_cmd) = match Self::get_sub_command(cmd_ctx) {
            Some((cmd_ctx, sub_cmd)) => (cmd_ctx, sub_cmd),
            None => return,
        };

        if caseless::canonical_caseless_match_str(&sub_cmd, "nodes") {
            let cluster_nodes = self
                .db
                .gen_cluster_nodes(cmd_ctx.get_db_name(), self.service_address.clone());
            cmd_ctx.set_resp_result(Ok(Resp::Bulk(BulkStr::Str(cluster_nodes.into_bytes()))))
        } else if caseless::canonical_caseless_match_str(&sub_cmd, "slots") {
            let cluster_slots = self
                .db
                .gen_cluster_slots(cmd_ctx.get_db_name(), self.service_address.clone());
            match cluster_slots {
                Ok(resp) => cmd_ctx.set_resp_result(Ok(resp)),
                Err(s) => cmd_ctx.set_resp_result(Ok(Resp::Error(s.into_bytes()))),
            }
        } else {
            cmd_ctx.set_resp_result(Ok(Resp::Error(
                String::from("Unsupported sub command").into_bytes(),
            )));
        }
    }

    fn get_sub_command(cmd_ctx: CmdCtx) -> Option<(CmdCtx, String)> {
        match cmd_ctx.get_cmd().get_key() {
            None => {
                cmd_ctx.set_resp_result(Ok(Resp::Error(
                    String::from("Missing sub command").into_bytes(),
                )));
                None
            }
            Some(ref k) => match str::from_utf8(k) {
                Ok(sub_cmd) => Some((cmd_ctx, sub_cmd.to_string())),
                Err(_) => {
                    cmd_ctx.set_resp_result(Ok(Resp::Error(
                        String::from("Invalid sub command").into_bytes(),
                    )));
                    None
                }
            },
        }
    }

    fn handle_umctl(&self, cmd_ctx: CmdCtx) {
        let (cmd_ctx, sub_cmd) = match Self::get_sub_command(cmd_ctx) {
            Some((cmd_ctx, sub_cmd)) => (cmd_ctx, sub_cmd),
            None => return,
        };

        let sub_cmd = sub_cmd.to_uppercase();

        if sub_cmd.eq("LISTDB") {
            let dbs = self.db.get_dbs();
            let resps = dbs
                .into_iter()
                .map(|db| Resp::Bulk(BulkStr::Str(db.into_bytes())))
                .collect();
            cmd_ctx.set_resp_result(Ok(Resp::Arr(Array::Arr(resps))));
        } else if sub_cmd.eq("CLEARDB") {
            self.db.clear();
            cmd_ctx.set_resp_result(Ok(Resp::Simple(String::from("OK").into_bytes())));
        } else if sub_cmd.eq("SETDB") {
            self.handle_umctl_setdb(cmd_ctx);
        } else if sub_cmd.eq("SETPEER") {
            self.handle_umctl_setpeer(cmd_ctx);
        } else if sub_cmd.eq("SETREPL") {
            self.handle_umctl_setrepl(cmd_ctx);
        } else if sub_cmd.eq("INFOREPL") {
            self.handle_umctl_info_repl(cmd_ctx);
        } else if sub_cmd.eq("INFOMGR") {
            self.handle_umctl_info_migration(cmd_ctx);
        } else if sub_cmd.eq("TMPSWITCH") {
            self.handle_umctl_tmp_switch(cmd_ctx);
        } else {
            cmd_ctx.set_resp_result(Ok(Resp::Error(
                String::from("Invalid sub command").into_bytes(),
            )));
        }
    }

    fn handle_umctl_setdb(&self, cmd_ctx: CmdCtx) {
        let db_map = match HostDBMap::from_resp(cmd_ctx.get_cmd().get_resp()) {
            Ok(db_map) => db_map,
            Err(_) => {
                cmd_ctx.set_resp_result(Ok(Resp::Error(
                    String::from("Invalid arguments").into_bytes(),
                )));
                return;
            }
        };

        let db_map_clone = db_map.clone();

        // Put db meta and migration meta together for consistency.
        // We can make sure that IMPORTING slots will not be handled directly
        // before the migration succeed. This is also why we should store the
        // new metadata to `migration_manager` first.
        match self.migration_manager.update(db_map_clone) {
            Ok(()) => {
                debug!("Successfully update migration meta data");
                debug!("local meta data: {:?}", db_map);
                match self.db.set_dbs(db_map) {
                    Ok(()) => {
                        cmd_ctx.set_resp_result(Ok(Resp::Simple("OK".to_string().into_bytes())));
                    }
                    Err(e) => {
                        //                        debug!("Failed to update local meta data {:?}", e);
                        match e {
                            DBError::OldEpoch => cmd_ctx.set_resp_result(Ok(Resp::Error(
                                OLD_EPOCH_REPLY.to_string().into_bytes(),
                            ))),
                        }
                    }
                }
            }
            Err(e) => {
                //                debug!("Failed to update migration meta data {:?}", e);
                match e {
                    DBError::OldEpoch => cmd_ctx
                        .set_resp_result(Ok(Resp::Error(OLD_EPOCH_REPLY.to_string().into_bytes()))),
                }
            }
        }
    }

    fn handle_umctl_setpeer(&self, cmd_ctx: CmdCtx) {
        let db_map = match HostDBMap::from_resp(cmd_ctx.get_cmd().get_resp()) {
            Ok(db_map) => db_map,
            Err(_) => {
                cmd_ctx.set_resp_result(Ok(Resp::Error(
                    String::from("Invalid arguments").into_bytes(),
                )));
                return;
            }
        };

        match self.db.set_peers(db_map) {
            Ok(()) => {
                info!("Successfully update peer meta data");
                cmd_ctx.set_resp_result(Ok(Resp::Simple(String::from("OK").into_bytes())));
            }
            Err(e) => {
                //                debug!("Failed to update peer meta data {:?}", e);
                match e {
                    DBError::OldEpoch => cmd_ctx
                        .set_resp_result(Ok(Resp::Error(OLD_EPOCH_REPLY.to_string().into_bytes()))),
                }
            }
        }
    }

    fn handle_umctl_setrepl(&self, cmd_ctx: CmdCtx) {
        let meta = match ReplicatorMeta::from_resp(cmd_ctx.get_cmd().get_resp()) {
            Ok(m) => m,
            Err(_) => {
                cmd_ctx.set_resp_result(Ok(Resp::Error(
                    String::from("Invalid arguments").into_bytes(),
                )));
                return;
            }
        };

        match self.replicator_manager.update_replicators(meta) {
            Ok(()) => {
                debug!("Successfully update replicator meta data");
                cmd_ctx.set_resp_result(Ok(Resp::Simple(String::from("OK").into_bytes())))
            }
            Err(e) => {
                //                debug!("Failed to update replicator meta data {:?}", e);
                match e {
                    DBError::OldEpoch => cmd_ctx
                        .set_resp_result(Ok(Resp::Error(OLD_EPOCH_REPLY.to_string().into_bytes()))),
                }
            }
        }
    }

    fn handle_umctl_info_repl(&self, cmd_ctx: CmdCtx) {
        let report = self.replicator_manager.get_metadata_report();
        cmd_ctx.set_resp_result(Ok(Resp::Bulk(BulkStr::Str(report.into_bytes()))));
    }

    fn handle_umctl_tmp_switch(&self, cmd_ctx: CmdCtx) {
        self.migration_manager.commit_importing(cmd_ctx);
    }

    fn handle_umctl_info_migration(&self, cmd_ctx: CmdCtx) {
        let finished_tasks = self.migration_manager.get_finished_tasks();
        let packet: Vec<Resp> = finished_tasks
            .into_iter()
            .map(|task| task.into_strings().join(" "))
            .map(|s| Resp::Bulk(BulkStr::Str(s.into_bytes())))
            .collect();
        cmd_ctx.set_resp_result(Ok(Resp::Arr(Array::Arr(packet))))
    }
}

impl<F: RedisClientFactory> CmdCtxHandler for ForwardHandler<F> {
    fn handle_cmd_ctx(&self, cmd_ctx: CmdCtx) {
        //        debug!("get command {:?}", cmd_ctx.get_cmd());
        let cmd_type = cmd_ctx.get_cmd().get_type();
        match cmd_type {
            CmdType::Ping => {
                cmd_ctx.set_resp_result(Ok(Resp::Simple(String::from("OK").into_bytes())))
            }
            CmdType::Info => cmd_ctx.set_resp_result(Ok(Resp::Bulk(BulkStr::Str(
                String::from("version:dev\r\n").into_bytes(),
            )))),
            CmdType::Auth => self.handle_auth(cmd_ctx),
            CmdType::Quit => {
                cmd_ctx.set_resp_result(Ok(Resp::Simple(String::from("OK").into_bytes())))
            }
            CmdType::Echo => {
                let req = cmd_ctx.get_cmd().get_resp().clone();
                cmd_ctx.set_resp_result(Ok(req))
            }
            CmdType::Select => {
                cmd_ctx.set_resp_result(Ok(Resp::Simple(String::from("OK").into_bytes())))
            }
            CmdType::Others => {
                let cmd_ctx = match self.migration_manager.send(cmd_ctx) {
                    Ok(()) => return,
                    Err(e) => match e {
                        DBSendError::SlotNotFound(cmd_ctx) => cmd_ctx,
                        err => {
                            error!("migration send task failed: {:?}", err);
                            return;
                        }
                    },
                };
                let res = self.db.send(cmd_ctx);
                if let Err(e) = res {
                    error!("Failed to foward cmd_ctx: {:?}", e)
                }
            }
            CmdType::Invalid => cmd_ctx.set_resp_result(Ok(Resp::Error(
                String::from("Invalid command").into_bytes(),
            ))),
            CmdType::UmCtl => self.handle_umctl(cmd_ctx),
            CmdType::Cluster => self.handle_cluster(cmd_ctx),
        };
    }
}
