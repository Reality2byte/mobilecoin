// Copyright (c) 2018-2022 The MobileCoin Foundation
#![deny(missing_docs)]

//! Recovery db implementation using a PostgreSQL database backend.

#[macro_use]
extern crate diesel;
extern crate diesel_migrations;

pub use error::Error;

pub mod test_utils;

mod error;
mod models;
mod proto_types;
mod schema;
mod sql_types;

use crate::sql_types::{SqlCompressedRistrettoPublic, UserEventType};
use ::prost::Message;
use chrono::NaiveDateTime;
use clap::Parser;
use diesel::{
    prelude::*,
    r2d2::{ConnectionManager, Pool},
};
use mc_attest_verifier_types::EvidenceKind;
use mc_blockchain_types::Block;
use mc_common::{
    logger::{log, Logger},
    HashMap,
};
use mc_crypto_keys::CompressedRistrettoPublic;
use mc_fog_kex_rng::KexRngPubkey;
use mc_fog_recovery_db_iface::{
    AddBlockDataStatus, ExpiredInvocationRecord, FogUserEvent, IngestInvocationId,
    IngressPublicKeyRecord, IngressPublicKeyRecordFilters, IngressPublicKeyStatus, RecoveryDb,
    RecoveryDbError, ReportData, ReportDb,
};
use mc_fog_types::{
    common::BlockRange,
    view::{FixedTxOutSearchResult, TxOutSearchResultCode},
    ETxOutRecord,
};
use mc_util_parse::parse_duration_in_seconds;
use proto_types::ProtoIngestedBlockData;
use retry::{delay, Error as RetryError, OperationResult};
use serde::Serialize;
use std::{cmp::max, time::Duration};

/// Maximum number of parameters PostgreSQL allows in a single query.
///
/// The actual limit is 65535. This value is more conservative, resulting on
/// potentially issueing more queries to the server. This is not expected to be
/// an issue.
pub const SQL_MAX_PARAMS: usize = 65000;

/// Maximal number of rows to insert in one batch.
pub const SQL_MAX_ROWS: usize = 5000;

/// SQL recovery DB connection configuration parameters
#[derive(Debug, Clone, Parser, Serialize)]
pub struct SqlRecoveryDbConnectionConfig {
    /// The idle timeout used by the connection pool.
    /// If set, connections will be closed after sitting idle for at most 30
    /// seconds beyond this duration. (https://docs.diesel.rs/diesel/r2d2/struct.Builder.html)
    #[clap(long, default_value = "60", value_parser = parse_duration_in_seconds, env = "MC_POSTGRES_IDLE_TIMEOUT")]
    pub postgres_idle_timeout: Duration,

    /// The maximum lifetime of connections in the pool.
    /// If set, connections will be closed after existing for at most 30 seconds
    /// beyond this duration. If a connection reaches its maximum lifetime
    /// while checked out it will be closed when it is returned to the pool. (https://docs.diesel.rs/diesel/r2d2/struct.Builder.html)
    #[clap(long, default_value = "120", value_parser = parse_duration_in_seconds, env = "MC_POSTGRES_MAX_LIFETIME")]
    pub postgres_max_lifetime: Duration,

    /// Sets the connection timeout used by the pool.
    /// The pool will wait this long for a connection to become available before
    /// returning an error. (https://docs.diesel.rs/diesel/r2d2/struct.Builder.html)
    #[clap(long, default_value = "5", value_parser = parse_duration_in_seconds, env = "MC_POSTGRES_CONNECTION_TIMEOUT")]
    pub postgres_connection_timeout: Duration,

    /// The maximum number of connections managed by the pool.
    #[clap(long, default_value = "1", env = "MC_POSTGRES_MAX_CONNECTIONS")]
    pub postgres_max_connections: u32,

    /// How many times to retry when we get retriable errors (connection /
    /// diesel errors)
    #[clap(long, default_value = "3", env = "MC_POSTGRES_RETRY_COUNT")]
    pub postgres_retry_count: usize,

    /// How long to back off (milliseconds) when we get retriable errors
    /// (connection / diesel errors)
    #[clap(long, default_value = "20", env = "MC_POSTGRES_RETRY_MILLIS")]
    pub postgres_retry_millis: u64,
}

impl Default for SqlRecoveryDbConnectionConfig {
    fn default() -> Self {
        Self {
            postgres_idle_timeout: Duration::from_secs(60),
            postgres_max_lifetime: Duration::from_secs(120),
            postgres_connection_timeout: Duration::from_secs(5),
            postgres_max_connections: 1,
            postgres_retry_count: 3,
            postgres_retry_millis: 20,
        }
    }
}

/// SQL-backed recovery database.
#[derive(Clone)]
pub struct SqlRecoveryDb {
    pool: Pool<ConnectionManager<PgConnection>>,
    config: SqlRecoveryDbConnectionConfig,
    logger: Logger,
}

impl SqlRecoveryDb {
    /// Create a new instance using a pre-existing connection pool.
    fn new(
        pool: Pool<ConnectionManager<PgConnection>>,
        config: SqlRecoveryDbConnectionConfig,
        logger: Logger,
    ) -> Self {
        Self {
            pool,
            config,
            logger,
        }
    }

    /// Create a new instance using a database URL,
    /// and connection parameters. The parameters have sane defaults.
    pub fn new_from_url(
        database_url: &str,
        config: SqlRecoveryDbConnectionConfig,
        logger: Logger,
    ) -> Result<Self, Error> {
        let manager = ConnectionManager::<PgConnection>::new(database_url);
        let pool = Pool::builder()
            .max_size(config.postgres_max_connections)
            .idle_timeout(Some(config.postgres_idle_timeout))
            .max_lifetime(Some(config.postgres_max_lifetime))
            .connection_timeout(config.postgres_connection_timeout)
            .test_on_check_out(true)
            .build(manager)?;
        Ok(Self::new(pool, config, logger))
    }

    // Helper function for retries config
    fn get_retries(&self) -> Box<dyn Iterator<Item = Duration>> {
        Box::new(
            delay::Fixed::from_millis(self.config.postgres_retry_millis)
                .take(self.config.postgres_retry_count)
                .map(delay::jitter),
        )
    }

    /// Mark a given ingest invocation as decommissioned.
    fn decommission_ingest_invocation_impl(
        &self,
        conn: &mut PgConnection,
        ingest_invocation_id: &IngestInvocationId,
    ) -> Result<(), Error> {
        // Mark the ingest invocation as decommissioned.
        diesel::update(
            schema::ingest_invocations::dsl::ingest_invocations
                .filter(schema::ingest_invocations::dsl::id.eq(**ingest_invocation_id)),
        )
        .set((
            schema::ingest_invocations::dsl::decommissioned.eq(true),
            schema::ingest_invocations::dsl::last_active_at.eq(diesel::dsl::now),
        ))
        .execute(conn)?;

        // Write a user event.
        let new_event =
            models::NewUserEvent::decommission_ingest_invocation(**ingest_invocation_id);

        diesel::insert_into(schema::user_events::table)
            .values(&new_event)
            .execute(conn)?;

        Ok(())
    }

    /// Mark a given ingest invocation as still being alive.
    fn update_last_active_at_impl(
        &self,
        conn: &mut PgConnection,
        ingest_invocation_id: &IngestInvocationId,
    ) -> Result<(), Error> {
        diesel::update(
            schema::ingest_invocations::dsl::ingest_invocations
                .filter(schema::ingest_invocations::dsl::id.eq(**ingest_invocation_id)),
        )
        .set(schema::ingest_invocations::dsl::last_active_at.eq(diesel::dsl::now))
        .execute(conn)?;

        Ok(())
    }

    /// Get missed block ranges.
    fn get_missed_block_ranges_impl(
        &self,
        conn: &mut PgConnection,
    ) -> Result<Vec<BlockRange>, Error> {
        let query = schema::user_events::dsl::user_events
            .filter(schema::user_events::dsl::event_type.eq(UserEventType::MissingBlocks))
            .select((
                schema::user_events::dsl::id,
                schema::user_events::dsl::missing_blocks_start,
                schema::user_events::dsl::missing_blocks_end,
            ))
            .order_by(schema::user_events::dsl::id);

        let rows = query.load::<(i64, Option<i64>, Option<i64>)>(conn)?;

        rows.iter()
            .map(|row| match row {
                (_, Some(start_index), Some(end_index)) => {
                    Ok(BlockRange::new(*start_index as u64, *end_index as u64))
                }
                (id, _, _) => Err(Error::UserEventSchemaViolation(
                    *id,
                    "missing start or end block indices",
                )),
            })
            .collect::<Result<Vec<BlockRange>, Error>>()
    }

    fn get_ingress_key_status_impl(
        &self,
        conn: &mut PgConnection,
        key: &CompressedRistrettoPublic,
    ) -> Result<Option<IngressPublicKeyStatus>, Error> {
        let key_bytes: &[u8] = key.as_ref();
        use schema::ingress_keys::dsl;
        let key_records: Vec<models::IngressKey> = dsl::ingress_keys
            .filter(dsl::ingress_public_key.eq(key_bytes))
            .load(conn)?;

        if key_records.is_empty() {
            Ok(None)
        } else if key_records.len() == 1 {
            Ok(Some(IngressPublicKeyStatus {
                start_block: key_records[0].start_block as u64,
                pubkey_expiry: key_records[0].pubkey_expiry as u64,
                retired: key_records[0].retired,
                lost: key_records[0].lost,
            }))
        } else {
            Err(Error::IngressKeysSchemaViolation(format!(
                "Found multiple entries for key: {key:?}"
            )))
        }
    }

    fn get_highest_known_block_index_impl(conn: &mut PgConnection) -> Result<Option<u64>, Error> {
        Ok(schema::ingested_blocks::dsl::ingested_blocks
            .select(diesel::dsl::max(schema::ingested_blocks::dsl::block_number))
            .first::<Option<i64>>(conn)?
            .map(|val| val as u64))
    }

    fn get_expired_invocations_impl(
        &self,
        conn: &mut PgConnection,
        expiration: NaiveDateTime,
    ) -> Result<Vec<ExpiredInvocationRecord>, Error> {
        use schema::ingest_invocations::dsl;
        let query = dsl::ingest_invocations
            .select((
                dsl::id,
                dsl::rng_version,
                dsl::egress_public_key,
                dsl::last_active_at,
            ))
            .filter(dsl::last_active_at.lt(expiration));
        let data = query.load::<(i64, i32, Vec<u8>, NaiveDateTime)>(conn)?;

        let result = data
            .into_iter()
            .map(|row| {
                let (ingest_invocation_id, rng_version, egress_public_key_bytes, last_active_at) =
                    row;

                let egress_kex_rng_pubkey = KexRngPubkey {
                    public_key: egress_public_key_bytes,
                    version: rng_version as u32,
                };

                ExpiredInvocationRecord {
                    ingest_invocation_id,
                    egress_public_key: egress_kex_rng_pubkey,
                    last_active_at,
                }
            })
            .collect();

        Ok(result)
    }

    ////
    // RecoveryDb functions that are meant to be retriable (don't take a conn as
    // argument)
    ////

    fn get_ingress_key_status_retriable(
        &self,
        key: &CompressedRistrettoPublic,
    ) -> Result<Option<IngressPublicKeyStatus>, Error> {
        let conn = &mut self.pool.get()?;
        self.get_ingress_key_status_impl(conn, key)
    }

    fn new_ingress_key_retriable(
        &self,
        key: &CompressedRistrettoPublic,
        start_block_count: u64,
    ) -> Result<u64, Error> {
        let conn = &mut self.pool.get()?;
        conn.build_transaction()
            .read_write()
            .run(|conn| -> Result<u64, Error> {
                let highest_known_block_count: u64 =
                    SqlRecoveryDb::get_highest_known_block_index_impl(conn)?
                        .map(|index| index + 1)
                        .unwrap_or(0);

                let accepted_start_block_count = max(start_block_count, highest_known_block_count);
                let obj = models::NewIngressKey {
                    ingress_public_key: (*key).into(),
                    start_block: accepted_start_block_count as i64,
                    pubkey_expiry: 0,
                    retired: false,
                    lost: false,
                };

                let inserted_row_count = diesel::insert_into(schema::ingress_keys::table)
                    .values(&obj)
                    .on_conflict_do_nothing()
                    .execute(conn)?;

                if inserted_row_count > 0 {
                    Ok(accepted_start_block_count)
                } else {
                    Err(Error::IngressKeyUnsuccessfulInsert(format!(
                        "Unable to insert ingress key: {key:?}"
                    )))
                }
            })
    }

    fn retire_ingress_key_retriable(
        &self,
        key: &CompressedRistrettoPublic,
        set_retired: bool,
    ) -> Result<(), Error> {
        let key_bytes: &[u8] = key.as_ref();

        let conn = &mut self.pool.get()?;
        use schema::ingress_keys::dsl;
        diesel::update(dsl::ingress_keys.filter(dsl::ingress_public_key.eq(key_bytes)))
            .set(dsl::retired.eq(set_retired))
            .execute(conn)?;
        Ok(())
    }

    fn get_last_scanned_block_index_retriable(
        &self,
        key: &CompressedRistrettoPublic,
    ) -> Result<Option<u64>, Error> {
        let key_bytes: &[u8] = key.as_ref();

        let conn = &mut self.pool.get()?;

        use schema::ingested_blocks::dsl;
        let maybe_index: Option<i64> = dsl::ingested_blocks
            .filter(dsl::ingress_public_key.eq(key_bytes))
            .select(diesel::dsl::max(dsl::block_number))
            .first(conn)?;

        Ok(maybe_index.map(|val| val as u64))
    }

    fn get_ingress_key_records_retriable(
        &self,
        start_block_at_least: u64,
        ingress_public_key_record_filters: &IngressPublicKeyRecordFilters,
    ) -> Result<Vec<IngressPublicKeyRecord>, Error> {
        let conn = &mut self.pool.get()?;

        use schema::ingress_keys::dsl;
        let last_scanned_block = diesel::dsl::sql::<diesel::sql_types::BigInt>(
                    "(SELECT MAX(block_number) FROM ingested_blocks WHERE ingress_keys.ingress_public_key = ingested_blocks.ingress_public_key)"
                );
        let mut query = dsl::ingress_keys
            .select((
                dsl::ingress_public_key,
                dsl::start_block,
                dsl::pubkey_expiry,
                dsl::retired,
                dsl::lost,
                last_scanned_block.clone().nullable(),
            ))
            .filter(dsl::start_block.ge(start_block_at_least as i64))
            // Allows for conditional queries, which means additional filter
            // clauses can be added to this query.
            .into_boxed();

        if ingress_public_key_record_filters.should_only_include_unexpired_keys {
            query = query
                .filter(last_scanned_block.clone().is_not_null())
                .filter(dsl::pubkey_expiry.gt(last_scanned_block));
        }
        if !ingress_public_key_record_filters.should_include_lost_keys {
            // Adds this filter to the existing query (rather than replacing it).
            query = query.filter(dsl::lost.eq(false));
        }

        if !ingress_public_key_record_filters.should_include_retired_keys {
            // Adds this filter to the existing query (rather than replacing it).
            query = query.filter(dsl::retired.eq(false));
        }

        // The list of fields here must match the .select() clause above.
        Ok(query
            .load::<(
                SqlCompressedRistrettoPublic,
                i64,
                i64,
                bool,
                bool,
                Option<i64>,
            )>(conn)?
            .into_iter()
            .map(
                |(
                    ingress_public_key,
                    start_block,
                    pubkey_expiry,
                    retired,
                    lost,
                    last_scanned_block,
                )| {
                    let status = IngressPublicKeyStatus {
                        start_block: start_block as u64,
                        pubkey_expiry: pubkey_expiry as u64,
                        retired,
                        lost,
                    };

                    IngressPublicKeyRecord {
                        key: *ingress_public_key,
                        status,
                        last_scanned_block: last_scanned_block.map(|v| v as u64),
                    }
                },
            )
            .collect())
    }

    fn new_ingest_invocation_retriable(
        &self,
        prev_ingest_invocation_id: Option<IngestInvocationId>,
        ingress_public_key: &CompressedRistrettoPublic,
        egress_public_key: &KexRngPubkey,
        start_block: u64,
    ) -> Result<IngestInvocationId, Error> {
        let conn = &mut self.pool.get()?;
        conn.build_transaction().read_write().run(|conn| {
            // Optionally decommission old invocation.
            if let Some(prev_ingest_invocation_id) = prev_ingest_invocation_id {
                self.decommission_ingest_invocation_impl(conn, &prev_ingest_invocation_id)?;
            }

            // Write new invocation.
            let now = diesel::select(diesel::dsl::now).get_result::<NaiveDateTime>(conn)?;

            let obj = models::NewIngestInvocation {
                ingress_public_key: (*ingress_public_key).into(),
                egress_public_key: egress_public_key.public_key.clone(),
                last_active_at: now,
                start_block: start_block as i64,
                decommissioned: false,
                rng_version: egress_public_key.version as i32,
            };

            let inserted_obj: models::IngestInvocation =
                diesel::insert_into(schema::ingest_invocations::table)
                    .values(&obj)
                    .get_result(conn)?;

            // Write a user event.
            let new_event = models::NewUserEvent::new_ingest_invocation(inserted_obj.id);

            diesel::insert_into(schema::user_events::table)
                .values(&new_event)
                .execute(conn)?;

            // Success.
            Ok(IngestInvocationId::from(inserted_obj.id))
        })
    }

    fn get_ingestable_ranges_retriable(
        &self,
    ) -> Result<Vec<mc_fog_recovery_db_iface::IngestableRange>, Error> {
        let conn = &mut self.pool.get()?;

        // For each ingest invocation we are aware of get its id, start block, is
        // decommissioned and the max block number it has ingested (if
        // available).
        let query = schema::ingest_invocations::dsl::ingest_invocations
            .select((
                schema::ingest_invocations::dsl::id,
                schema::ingest_invocations::dsl::start_block,
                schema::ingest_invocations::dsl::decommissioned,
                diesel::dsl::sql::<diesel::sql_types::BigInt>(
                    "(SELECT MAX(block_number) FROM ingested_blocks WHERE ingest_invocations.id = ingested_blocks.ingest_invocation_id)"
                ).nullable(),
            ))
            .order_by(schema::ingest_invocations::dsl::id);

        // The list of fields here must match the .select() clause above.
        let data = query.load::<(i64, i64, bool, Option<i64>)>(conn)?;
        Ok(data
            .into_iter()
            .map(|row| {
                let (ingest_invocation_id, start_block, decommissioned, last_ingested_block) = row;

                mc_fog_recovery_db_iface::IngestableRange {
                    id: IngestInvocationId::from(ingest_invocation_id),
                    start_block: start_block as u64,
                    decommissioned,
                    last_ingested_block: last_ingested_block.map(|v| v as u64),
                }
            })
            .collect())
    }

    /// Decommission a given ingest invocation.
    ///
    /// This should be done when a given ingest enclave goes down or is retired.
    ///
    /// Arguments:
    /// * ingest_invocation_id: The unique ingest invocation id that has been
    ///   retired
    fn decommission_ingest_invocation_retriable(
        &self,
        ingest_invocation_id: &IngestInvocationId,
    ) -> Result<(), Error> {
        let conn = &mut self.pool.get()?;

        conn.build_transaction()
            .read_write()
            .run(|conn| self.decommission_ingest_invocation_impl(conn, ingest_invocation_id))
    }

    fn add_block_data_retriable(
        &self,
        ingest_invocation_id: &IngestInvocationId,
        block: &Block,
        block_signature_timestamp: u64,
        txs: &[mc_fog_types::ETxOutRecord],
    ) -> Result<AddBlockDataStatus, Error> {
        let conn = &mut self.pool.get()?;

        let res = conn
            .build_transaction()
            .read_write()
            .run(|conn| -> Result<(), Error> {
                // Get ingress pubkey of this ingest invocation id, which is also stored in the
                // ingested_block record
                //
                // Note: Possibly, we can use an inner-join or something when we would have
                // needed this, and then not have this in the ingest_blocks
                // table? It makes the sql expressions simpler for now, we could
                // delete that column from table later
                let ingress_key_bytes: Vec<u8> = schema::ingest_invocations::table
                    .filter(schema::ingest_invocations::dsl::id.eq(**ingest_invocation_id))
                    .select(schema::ingest_invocations::ingress_public_key)
                    .first(conn)?;

                // Get bytes of encoded proto ingested block data
                let proto_bytes = {
                    let proto_ingested_block_data = ProtoIngestedBlockData {
                        e_tx_out_records: txs.to_vec(),
                    };
                    proto_ingested_block_data.encode_to_vec()
                };

                // Add an IngestedBlock record.
                let new_ingested_block = models::NewIngestedBlock {
                    ingress_public_key: ingress_key_bytes,
                    ingest_invocation_id: **ingest_invocation_id,
                    block_number: block.index as i64,
                    cumulative_txo_count: block.cumulative_txo_count as i64,
                    block_signature_timestamp: block_signature_timestamp as i64,
                    proto_ingested_block_data: proto_bytes,
                };

                diesel::insert_into(schema::ingested_blocks::table)
                    .values(&new_ingested_block)
                    .execute(conn)?;

                // Update last active at.
                self.update_last_active_at_impl(conn, ingest_invocation_id)?;

                // Success.
                Ok(())
            });

        match res {
            Ok(()) => Ok(AddBlockDataStatus {
                block_already_scanned_with_this_key: false,
            }),
            // If a unique constraint is violated, we return Ok(block_already_scanned: true) instead
            // of an error This makes it a little easier for the caller to access this
            // information without making custom traits for interrogating generic
            // errors.
            Err(Error::Orm(diesel::result::Error::DatabaseError(
                diesel::result::DatabaseErrorKind::UniqueViolation,
                details,
            ))) => {
                log::info!(self.logger, "Unique constraint violated when adding block {} for ingest invocation id {}: {:?}", block.index, ingest_invocation_id, details);
                Ok(AddBlockDataStatus {
                    block_already_scanned_with_this_key: true,
                })
            }
            Err(err) => Err(err),
        }
    }

    fn report_lost_ingress_key_retriable(
        &self,
        lost_ingress_key: CompressedRistrettoPublic,
    ) -> Result<(), Error> {
        let conn = &mut self.pool.get()?;

        conn.build_transaction().read_write().run(|conn| {
            // Find the ingress key and update it to be marked lost
            let key_bytes: &[u8] = lost_ingress_key.as_ref();
            use schema::ingress_keys::dsl;
            let key_records: Vec<models::IngressKey> =
                diesel::update(dsl::ingress_keys.filter(dsl::ingress_public_key.eq(key_bytes)))
                    .set(dsl::lost.eq(true))
                    .get_results(conn)?;

            // Compute a missed block range based on looking at the key status,
            // which is correct if no blocks have actually been scanned using the key.
            let mut missed_block_range = if key_records.is_empty() {
                return Err(Error::MissingIngressKey(lost_ingress_key));
            } else if key_records.len() == 1 {
                BlockRange {
                    start_block: key_records[0].start_block as u64,
                    end_block: key_records[0].pubkey_expiry as u64,
                }
            } else {
                return Err(Error::IngressKeysSchemaViolation(format!(
                    "Found multiple entries for key: {lost_ingress_key:?}"
                )));
            };

            // Find the last scanned block index (if any block has been scanned with this
            // key)
            let maybe_block_index: Option<i64> = {
                use schema::ingested_blocks::dsl;
                dsl::ingested_blocks
                    .filter(dsl::ingress_public_key.eq(key_bytes))
                    .select(diesel::dsl::max(dsl::block_number))
                    .first(conn)?
            };

            if let Some(block_index) = maybe_block_index {
                let block_index = block_index as u64;
                if block_index + 1 >= missed_block_range.end_block {
                    // There aren't actually any blocks that need to be scanned, so we are done
                    // without creating a user event.
                    return Ok(());
                }
                // If we did actually scan some blocks, then report a smaller range
                if block_index + 1 > missed_block_range.start_block {
                    missed_block_range.start_block = block_index + 1;
                }
            }

            // If the missed block range is invalid (empty), we don't have to add it.
            // This can happen if the ingress key was never actually published to the report
            // server, and then pubkey_expiry is zero.
            if !missed_block_range.is_valid() {
                return Ok(());
            }

            // Add new range.
            let new_event = models::NewUserEvent::missing_blocks(&missed_block_range);

            diesel::insert_into(schema::user_events::table)
                .values(&new_event)
                .execute(conn)?;

            Ok(())
        })
    }

    fn get_missed_block_ranges_retriable(&self) -> Result<Vec<BlockRange>, Error> {
        let conn = &mut self.pool.get()?;
        self.get_missed_block_ranges_impl(conn)
    }

    fn search_user_events_retriable(
        &self,
        start_from_user_event_id: i64,
    ) -> Result<(Vec<FogUserEvent>, i64), Error> {
        // Early return if start_from_user_event_id is max
        if start_from_user_event_id == i64::MAX {
            return Ok((Default::default(), i64::MAX));
        }

        let conn = &mut self.pool.get()?;
        let mut events: Vec<(i64, FogUserEvent)> = Vec::new();

        // Collect all events of interest
        let query = schema::user_events::dsl::user_events
            // Left-join ingest invocation information, needed for NewRngRecord events
            .left_join(
                schema::ingest_invocations::dsl::ingest_invocations.on(
                    schema::user_events::dsl::new_ingest_invocation_id.eq(
                        schema::ingest_invocations::dsl::id.nullable()
                    )
                )
            )

            // Filtered by the subset of ids we are exploring
            // NOTE: sql auto increment columns start from 1, so "start_from_user_event_id = 0"
            // will capture everything
            .filter(schema::user_events::dsl::id.gt(start_from_user_event_id))
            // Get only the fields that we need
            .select((
                // Fields for every event type
                schema::user_events::dsl::id,
                schema::user_events::dsl::event_type,
                // Fields for NewIngestInvocation events
                schema::ingest_invocations::dsl::id.nullable(),
                schema::ingest_invocations::dsl::egress_public_key.nullable(),
                schema::ingest_invocations::dsl::rng_version.nullable(),
                schema::ingest_invocations::dsl::start_block.nullable(),
                // Fields for DecommissionIngestInvocation
                schema::user_events::dsl::decommission_ingest_invocation_id,
                diesel::dsl::sql::<diesel::sql_types::BigInt>("(SELECT COALESCE(MAX(block_number), 0) FROM ingested_blocks WHERE user_events.event_type = 'decommission_ingest_invocation' AND ingested_blocks.ingest_invocation_id = user_events.decommission_ingest_invocation_id)"),
                // Fields for MissingBlocks events
                schema::user_events::dsl::missing_blocks_start,
                schema::user_events::dsl::missing_blocks_end,
            ));

        // The list of fields here must match the .select() clause above.
        let data = query.load::<(
            // For all event types
            i64,           // user_events.id
            UserEventType, // user_events.event_type
            // For NewRngRecord events
            Option<i64>,     // rng_record.ingest_invocation_id
            Option<Vec<u8>>, // rng_record.egress_public_key
            Option<i32>,     // rng_record.rng_version
            Option<i64>,     // rng_record.start_block
            // For DecommissionIngestInvocation events
            Option<i64>, // ingest_invocations.id
            i64,         // MAX(ingested_blocks.block_number)
            // For MissingBlocks events
            Option<i64>, // user_events.missing_blocks_start
            Option<i64>, // user_events.missing_blocks_end
        )>(conn)?;

        // If no events are found, return start_from_user_event_id and not 0
        let mut max_user_event_id = start_from_user_event_id;
        for row in data.into_iter() {
            // The list of fields here must match the .select() clause above.
            let (
                user_event_id,
                user_event_type,
                rng_record_ingest_invocation_id,
                rng_record_egress_public_key,
                rng_record_rng_version,
                rng_record_start_block,
                decommission_ingest_invocation_id,
                decommission_ingest_invocation_max_block,
                missing_blocks_start,
                missing_blocks_end,
            ) = row;

            // Update running max
            max_user_event_id = core::cmp::max(max_user_event_id, user_event_id);

            events.push((
                user_event_id,
                match user_event_type {
                    UserEventType::NewIngestInvocation => {
                        FogUserEvent::NewRngRecord(mc_fog_types::view::RngRecord {
                            ingest_invocation_id: rng_record_ingest_invocation_id.ok_or(
                                Error::UserEventSchemaViolation(
                                    user_event_id,
                                    "missing rng_record_ingest_invocation_id",
                                ),
                            )?,
                            pubkey: mc_fog_types::view::KexRngPubkey {
                                public_key: rng_record_egress_public_key.ok_or(
                                    Error::UserEventSchemaViolation(
                                        user_event_id,
                                        "missing rng_record_egress_public_key",
                                    ),
                                )?,
                                version: rng_record_rng_version.ok_or(
                                    Error::UserEventSchemaViolation(
                                        user_event_id,
                                        "missing rng_record_rng_version",
                                    ),
                                )? as u32,
                            },
                            start_block: rng_record_start_block.ok_or(
                                Error::UserEventSchemaViolation(
                                    user_event_id,
                                    "missing rng_record_start_block",
                                ),
                            )? as u64,
                        })
                    }
                    UserEventType::DecommissionIngestInvocation => {
                        FogUserEvent::DecommissionIngestInvocation(
                            mc_fog_types::view::DecommissionedIngestInvocation {
                                ingest_invocation_id: decommission_ingest_invocation_id.ok_or(
                                    Error::UserEventSchemaViolation(
                                        user_event_id,
                                        "missing decommission_ingest_invocation_id",
                                    ),
                                )?,
                                last_ingested_block: decommission_ingest_invocation_max_block
                                    as u64,
                            },
                        )
                    }
                    UserEventType::MissingBlocks => {
                        FogUserEvent::MissingBlocks(mc_fog_types::common::BlockRange {
                            start_block: missing_blocks_start.ok_or(
                                Error::UserEventSchemaViolation(
                                    user_event_id,
                                    "missing missing_blocks_start",
                                ),
                            )? as u64,
                            end_block: missing_blocks_end.ok_or(Error::UserEventSchemaViolation(
                                user_event_id,
                                "missing missing_blocks_end",
                            ))? as u64,
                        })
                    }
                },
            ));
        }

        // Ensure events are properly sorted.
        events.sort_by_key(|(id, _event)| *id);

        // Return.
        Ok((
            events.into_iter().map(|(_event_id, event)| event).collect(),
            max_user_event_id,
        ))
    }

    /// Get any TxOutSearchResults corresponding to given search keys.
    /// Nonzero start_block can be provided as an optimization opportunity.
    ///
    /// Note: This is still supported for some tests, but it is VERY SLOW.
    /// We no longer have an index for ETxOutRecords by search key in the SQL
    /// directly. This should not be used except in tests.
    ///
    /// Arguments:
    /// * start_block: A lower bound on where we will search. This can often be
    ///   provided by the user in order to limit the scope of the search and
    ///   reduce load on the servers.
    /// * search_keys: A list of fog tx_out search keys to search for.
    ///
    /// Returns:
    /// * Exactly one FixedTxOutSearchResult object for every search key, or an
    ///   internal database error description.
    fn get_tx_outs_retriable(
        &self,
        start_block: u64,
        search_keys: &[Vec<u8>],
    ) -> Result<Vec<FixedTxOutSearchResult>, Error> {
        let conn = &mut self.pool.get()?;

        let query = schema::ingested_blocks::dsl::ingested_blocks
            .filter(schema::ingested_blocks::dsl::block_number.ge(start_block as i64))
            .select(schema::ingested_blocks::dsl::proto_ingested_block_data);

        let mut search_key_to_payload = HashMap::<Vec<u8>, Vec<u8>>::default();
        for proto_bytes in query.load::<Vec<u8>>(conn)? {
            let proto = ProtoIngestedBlockData::decode(&*proto_bytes)?;
            for e_tx_out_record in proto.e_tx_out_records {
                search_key_to_payload.insert(e_tx_out_record.search_key, e_tx_out_record.payload);
            }
        }

        let mut results = Vec::new();
        for search_key in search_keys {
            results.push(match search_key_to_payload.get(search_key) {
                Some(payload) => FixedTxOutSearchResult::new(
                    search_key.clone(),
                    payload,
                    TxOutSearchResultCode::Found,
                ),
                None => FixedTxOutSearchResult::new_not_found(search_key.clone()),
            });
        }

        Ok(results)
    }

    /// Mark a given ingest invocation as still being alive.
    fn update_last_active_at_retriable(
        &self,
        ingest_invocation_id: &IngestInvocationId,
    ) -> Result<(), Error> {
        let conn = &mut self.pool.get()?;
        self.update_last_active_at_impl(conn, ingest_invocation_id)
    }

    /// Get any ETxOutRecords produced by a given ingress key for a given
    /// block index.
    ///
    /// Arguments:
    /// * ingress_key: The ingress key we need ETxOutRecords from
    /// * block_index: The block we need ETxOutRecords from
    ///
    /// Returns:
    /// * The ETxOutRecord's from when this block was added, or None if the
    ///   block doesn't exist yet or, an error
    fn get_tx_outs_by_block_and_key_retriable(
        &self,
        ingress_key: CompressedRistrettoPublic,
        block_index: u64,
    ) -> Result<Option<Vec<ETxOutRecord>>, Error> {
        let conn = &mut self.pool.get()?;

        let key_bytes: &[u8] = ingress_key.as_ref();
        let query = schema::ingested_blocks::dsl::ingested_blocks
            .filter(schema::ingested_blocks::dsl::ingress_public_key.eq(key_bytes))
            .filter(schema::ingested_blocks::dsl::block_number.eq(block_index as i64))
            .select(schema::ingested_blocks::dsl::proto_ingested_block_data);

        // The result of load should be 0 or 1, since there is a database constraint
        // around ingress keys and block indices
        let protos: Vec<Vec<u8>> = query.load::<Vec<u8>>(conn)?;

        if protos.is_empty() {
            Ok(None)
        } else if protos.len() == 1 {
            let proto = ProtoIngestedBlockData::decode(&*protos[0])?;
            Ok(Some(proto.e_tx_out_records))
        } else {
            Err(Error::IngestedBlockSchemaViolation(format!("Found {} different entries for ingress_key {:?} and block_index {}, which goes against the constraint", protos.len(), ingress_key, block_index)))
        }
    }

    /// Get ETxOutRecords for a given ingress key from a block, and subsequent
    /// blocks, up to some limit. (This is like a batch call to
    /// get_tx_outs_by_block_and_key_retriable with lookahead, and makes
    /// sense if there is high network latency.)
    ///
    /// Arguments:
    /// * ingress_key: The ingress key we need ETxOutRecords from
    /// * block_range: The range of blocks to get ETxOutRecords from.
    ///
    /// Returns:
    /// * The sequence of ETxOutRecord's, from consecutive blocks starting from
    ///   block_index. Empty if not even the block_index'th block exists.
    fn get_tx_outs_by_block_range_and_key_retriable(
        &self,
        ingress_key: CompressedRistrettoPublic,
        block_range: &BlockRange,
    ) -> Result<Vec<Vec<ETxOutRecord>>, Error> {
        let conn = &mut self.pool.get()?;

        // The idea is:
        // Similar to get_tx_outs_by_block_and_key_retriable, but now
        // * we have a range of admissible block indices
        // * we order by the block number
        // * we also select over the block number so that sql gives us the block number
        //
        // This ensures that we can detect any gaps in the data
        let key_bytes: &[u8] = ingress_key.as_ref();
        let query = {
            use schema::ingested_blocks::dsl;
            dsl::ingested_blocks
                .filter(dsl::ingress_public_key.eq(key_bytes))
                .filter(dsl::block_number.ge(block_range.start_block as i64))
                .limit(block_range.len() as i64)
                .select((dsl::block_number, dsl::proto_ingested_block_data))
                .order(dsl::block_number.asc())
        };

        // We will get one row for each hit in the table we found
        let rows: Vec<(i64, Vec<u8>)> = query.load(conn)?;

        if (rows.len() as u64) > block_range.len() {
            log::warn!(
                self.logger,
                "When querying, more responses than expected: {} > {}",
                rows.len(),
                block_range.len(),
            );
        }

        // We want to iterate over the rows we got, make sure there are no gaps in block
        // indices, and decode the TxOut's and return them. If there are gaps,
        // we log at warn level, and short-circuit out of this, returning only
        // whatever we managed to get. That will discard data that we got from
        // the DB and we will request it again later, but there is no reason for
        // there to be gaps, that's not how the system works, so it isn't
        // important to optimize for that case.

        let mut result = Vec::new();
        for (idx, (block_number, proto)) in rows.into_iter().enumerate() {
            if block_range.start_block + (idx as u64) == block_number as u64 {
                let proto = ProtoIngestedBlockData::decode(&*proto)?;
                result.push(proto.e_tx_out_records);
            } else {
                log::warn!(self.logger, "When querying for block index {} and up to {} blocks on, the {}'th response has block_number {} which is not expected. Gaps in the data?", block_range.start_block, block_range.len(), idx, block_number);
                break;
            }
        }
        Ok(result)
    }

    /// Get iid that produced data for given ingress key and a given block
    /// index.
    fn get_invocation_id_by_block_and_key_retriable(
        &self,
        ingress_key: CompressedRistrettoPublic,
        block_index: u64,
    ) -> Result<Option<IngestInvocationId>, Error> {
        let conn = &mut self.pool.get()?;

        let key_bytes: &[u8] = ingress_key.as_ref();
        let query = schema::ingested_blocks::dsl::ingested_blocks
            .filter(schema::ingested_blocks::dsl::ingress_public_key.eq(key_bytes))
            .filter(schema::ingested_blocks::dsl::block_number.eq(block_index as i64))
            .select(schema::ingested_blocks::dsl::ingest_invocation_id);

        // The result of load should be 0 or 1, since there is a database constraint
        // around ingress keys and block indices
        let iids: Vec<i64> = query.load::<i64>(conn)?;

        if iids.is_empty() {
            Ok(None)
        } else if iids.len() == 1 {
            Ok(Some(iids[0].into()))
        } else {
            Err(Error::IngestedBlockSchemaViolation(format!("Found {} different entries for ingress_key {:?} and block_index {}, which goes against the constraint", iids.len(), ingress_key, block_index)))
        }
    }

    /// Get the cumulative txo count for a given block number.
    ///
    /// Arguments:
    /// * block_index: The block we need cumulative_txo_count for.
    ///
    /// Returns:
    /// * Some(cumulative_txo_count) if the block was found in the database,
    ///   None if it wasn't, or an error if the query failed.
    fn get_cumulative_txo_count_for_block_retriable(
        &self,
        block_index: u64,
    ) -> Result<Option<u64>, Error> {
        let conn = &mut self.pool.get()?;

        let query = schema::ingested_blocks::dsl::ingested_blocks
            .filter(schema::ingested_blocks::dsl::block_number.eq(block_index as i64))
            .select(schema::ingested_blocks::dsl::cumulative_txo_count);

        let data = query.load::<i64>(conn)?;
        if data.is_empty() {
            Ok(None)
        } else {
            let cumulative_txo_count = data[0];
            if data.iter().all(|val| *val == cumulative_txo_count) {
                Ok(Some(cumulative_txo_count as u64))
            } else {
                Err(Error::IngestedBlockSchemaViolation(format!(
                    "Found multiple cumulative_txo_count values for block {block_index}: {data:?}"
                )))
            }
        }
    }

    /// Get the block signature timestamp for a given block number.
    /// Note that it is unspecified which timestamp we use if there are multiple
    /// timestamps.
    ///
    /// Arguments:
    /// * block_index: The block we need timestamp for.
    ///
    /// Returns:
    /// * Some(cumulative_txo_count) if the block was found in the database,
    ///   None if it wasn't, or an error if the query failed.
    fn get_block_signature_timestamp_for_block_retriable(
        &self,
        block_index: u64,
    ) -> Result<Option<u64>, Error> {
        let conn = &mut self.pool.get()?;

        let query = schema::ingested_blocks::dsl::ingested_blocks
            .filter(schema::ingested_blocks::dsl::block_number.eq(block_index as i64))
            .select(schema::ingested_blocks::dsl::block_signature_timestamp);

        let data = query.load::<i64>(conn)?;
        Ok(data.first().map(|val| *val as u64))
    }

    /// Get the highest block index for which we have any data at all.
    fn get_highest_known_block_index_retriable(&self) -> Result<Option<u64>, Error> {
        let conn = &mut self.pool.get()?;
        SqlRecoveryDb::get_highest_known_block_index_impl(conn)
    }

    ////
    // ReportDb functions that are meant to be retriable (don't take a conn as
    // argument)
    ////

    fn get_all_reports_retriable(&self) -> Result<Vec<(String, ReportData)>, Error> {
        let conn = &mut self.pool.get()?;

        let query = schema::reports::dsl::reports
            .select((
                schema::reports::dsl::ingest_invocation_id,
                schema::reports::dsl::fog_report_id,
                schema::reports::dsl::report,
                schema::reports::dsl::pubkey_expiry,
            ))
            .order_by(schema::reports::dsl::id);

        query
            .load::<(Option<i64>, String, Vec<u8>, i64)>(conn)?
            .into_iter()
            .map(|(ingest_invocation_id, report_id, report, pubkey_expiry)| {
                let attestation_evidence = EvidenceKind::from_bytes(report)?;
                Ok((
                    report_id,
                    ReportData {
                        ingest_invocation_id: ingest_invocation_id.map(IngestInvocationId::from),
                        attestation_evidence: attestation_evidence.into(),
                        pubkey_expiry: pubkey_expiry as u64,
                    },
                ))
            })
            .collect()
    }

    /// Set report data associated with a given report id.
    fn set_report_retriable(
        &self,
        ingress_key: &CompressedRistrettoPublic,
        report_id: &str,
        data: &ReportData,
    ) -> Result<IngressPublicKeyStatus, Error> {
        let conn = &mut self.pool.get()?;

        conn.build_transaction()
            .read_write()
            .run(|conn| -> Result<IngressPublicKeyStatus, Error> {
                // First, try to update the pubkey_expiry value on this ingress key, only
                // allowing it to increase, and only if it is not retired
                let result: IngressPublicKeyStatus = {
                    let key_bytes: &[u8] = ingress_key.as_ref();

                    use schema::ingress_keys::dsl;
                    let key_records: Vec<models::IngressKey> = diesel::update(
                        dsl::ingress_keys
                            .filter(dsl::ingress_public_key.eq(key_bytes))
                            .filter(dsl::retired.eq(false))
                            .filter(dsl::pubkey_expiry.lt(data.pubkey_expiry as i64)),
                    )
                    .set(dsl::pubkey_expiry.eq(data.pubkey_expiry as i64))
                    .get_results(conn)?;

                    if key_records.is_empty() {
                        // If the result is empty, the key might not exist, or it might have had a
                        // larger pubkey expiry (because this server is behind),
                        // so we need to make another query to find which is the case
                        log::info!(self.logger, "update was a no-op");
                        let maybe_key_status =
                            self.get_ingress_key_status_impl(conn, ingress_key)?;
                        log::info!(self.logger, "check ingress key passed");
                        maybe_key_status.ok_or(Error::MissingIngressKey(*ingress_key))?
                    } else if key_records.len() > 1 {
                        return Err(Error::IngressKeysSchemaViolation(format!(
                            "Found multiple entries for key: {:?}",
                            *ingress_key
                        )));
                    } else {
                        IngressPublicKeyStatus {
                            start_block: key_records[0].start_block as u64,
                            pubkey_expiry: key_records[0].pubkey_expiry as u64,
                            retired: key_records[0].retired,
                            lost: key_records[0].lost,
                        }
                    }
                };

                log::info!(self.logger, "Got status for key: {:?}", result);
                if result.retired {
                    log::info!(self.logger, "Cannot publish key because it is retired");
                    return Ok(result);
                }

                let report_bytes =
                    EvidenceKind::from(data.attestation_evidence.clone()).into_bytes();
                let report = models::NewReport {
                    ingress_public_key: ingress_key.as_ref(),
                    ingest_invocation_id: data.ingest_invocation_id.map(i64::from),
                    fog_report_id: report_id,
                    report: report_bytes.as_slice(),
                    pubkey_expiry: data.pubkey_expiry as i64,
                };

                diesel::insert_into(schema::reports::dsl::reports)
                    .values(&report)
                    .on_conflict(schema::reports::dsl::fog_report_id)
                    .do_update()
                    .set((
                        schema::reports::dsl::ingress_public_key.eq(report.ingress_public_key),
                        schema::reports::dsl::ingest_invocation_id.eq(report.ingest_invocation_id),
                        schema::reports::dsl::report.eq(report_bytes.clone()),
                        schema::reports::dsl::pubkey_expiry.eq(report.pubkey_expiry),
                    ))
                    .execute(conn)?;
                Ok(result)
            })
    }

    /// Remove report data associated with a given report id.
    fn remove_report_retriable(&self, report_id: &str) -> Result<(), Error> {
        let conn = &mut self.pool.get()?;
        diesel::delete(
            schema::reports::dsl::reports.filter(schema::reports::dsl::fog_report_id.eq(report_id)),
        )
        .execute(conn)?;
        Ok(())
    }

    fn get_expired_invocations_retriable(
        &self,
        expiration: NaiveDateTime,
    ) -> Result<Vec<ExpiredInvocationRecord>, Error> {
        let conn = &mut self.pool.get()?;
        self.get_expired_invocations_impl(conn, expiration)
    }
}

/// See trait `fog_recovery_db_iface::RecoveryDb` for documentation.
impl RecoveryDb for SqlRecoveryDb {
    type Error = Error;

    fn get_ingress_key_status(
        &self,
        key: &CompressedRistrettoPublic,
    ) -> Result<Option<IngressPublicKeyStatus>, Self::Error> {
        our_retry(self.get_retries(), || {
            self.get_ingress_key_status_retriable(key)
        })
    }

    fn new_ingress_key(
        &self,
        key: &CompressedRistrettoPublic,
        start_block_count: u64,
    ) -> Result<u64, Self::Error> {
        our_retry(self.get_retries(), || {
            self.new_ingress_key_retriable(key, start_block_count)
        })
    }

    fn retire_ingress_key(
        &self,
        key: &CompressedRistrettoPublic,
        set_retired: bool,
    ) -> Result<(), Self::Error> {
        our_retry(self.get_retries(), || {
            self.retire_ingress_key_retriable(key, set_retired)
        })
    }

    fn get_last_scanned_block_index(
        &self,
        key: &CompressedRistrettoPublic,
    ) -> Result<Option<u64>, Self::Error> {
        our_retry(self.get_retries(), || {
            self.get_last_scanned_block_index_retriable(key)
        })
    }

    fn get_ingress_key_records(
        &self,
        start_block_at_least: u64,
        ingress_public_key_record_filters: &IngressPublicKeyRecordFilters,
    ) -> Result<Vec<IngressPublicKeyRecord>, Self::Error> {
        our_retry(self.get_retries(), || {
            self.get_ingress_key_records_retriable(
                start_block_at_least,
                ingress_public_key_record_filters,
            )
        })
    }

    fn new_ingest_invocation(
        &self,
        prev_ingest_invocation_id: Option<IngestInvocationId>,
        ingress_public_key: &CompressedRistrettoPublic,
        egress_public_key: &KexRngPubkey,
        start_block: u64,
    ) -> Result<IngestInvocationId, Self::Error> {
        our_retry(self.get_retries(), || {
            self.new_ingest_invocation_retriable(
                prev_ingest_invocation_id,
                ingress_public_key,
                egress_public_key,
                start_block,
            )
        })
    }

    fn get_ingestable_ranges(
        &self,
    ) -> Result<Vec<mc_fog_recovery_db_iface::IngestableRange>, Self::Error> {
        our_retry(self.get_retries(), || {
            self.get_ingestable_ranges_retriable()
        })
    }

    /// Decommission a given ingest invocation.
    ///
    /// This should be done when a given ingest enclave goes down or is retired.
    ///
    /// Arguments:
    /// * ingest_invocation_id: The unique ingest invocation id that has been
    ///   retired
    fn decommission_ingest_invocation(
        &self,
        ingest_invocation_id: &IngestInvocationId,
    ) -> Result<(), Self::Error> {
        our_retry(self.get_retries(), || {
            self.decommission_ingest_invocation_retriable(ingest_invocation_id)
        })
    }

    fn add_block_data(
        &self,
        ingest_invocation_id: &IngestInvocationId,
        block: &Block,
        block_signature_timestamp: u64,
        txs: &[mc_fog_types::ETxOutRecord],
    ) -> Result<AddBlockDataStatus, Self::Error> {
        our_retry(self.get_retries(), || {
            self.add_block_data_retriable(
                ingest_invocation_id,
                block,
                block_signature_timestamp,
                txs,
            )
        })
    }

    fn report_lost_ingress_key(
        &self,
        lost_ingress_key: CompressedRistrettoPublic,
    ) -> Result<(), Self::Error> {
        our_retry(self.get_retries(), || {
            self.report_lost_ingress_key_retriable(lost_ingress_key)
        })
    }

    fn get_missed_block_ranges(&self) -> Result<Vec<BlockRange>, Self::Error> {
        our_retry(self.get_retries(), || {
            self.get_missed_block_ranges_retriable()
        })
    }

    fn search_user_events(
        &self,
        start_from_user_event_id: i64,
    ) -> Result<(Vec<FogUserEvent>, i64), Self::Error> {
        our_retry(self.get_retries(), || {
            self.search_user_events_retriable(start_from_user_event_id)
        })
    }

    /// Get any TxOutSearchResults corresponding to given search keys.
    /// Nonzero start_block can be provided as an optimization opportunity.
    ///
    /// Note: This is still supported for some tests, but it is VERY SLOW.
    /// We no longer have an index for ETxOutRecords by search key in the SQL
    /// directly. This should not be used except in tests.
    ///
    /// Arguments:
    /// * start_block: A lower bound on where we will search. This can often be
    ///   provided by the user in order to limit the scope of the search and
    ///   reduce load on the servers.
    /// * search_keys: A list of fog tx_out search keys to search for.
    ///
    /// Returns:
    /// * Exactly one FixedTxOutSearchResult object for every search key, or an
    ///   internal database error description.
    fn get_tx_outs(
        &self,
        start_block: u64,
        search_keys: &[Vec<u8>],
    ) -> Result<Vec<FixedTxOutSearchResult>, Self::Error> {
        our_retry(self.get_retries(), || {
            self.get_tx_outs_retriable(start_block, search_keys)
        })
    }

    /// Mark a given ingest invocation as still being alive.
    fn update_last_active_at(
        &self,
        ingest_invocation_id: &IngestInvocationId,
    ) -> Result<(), Self::Error> {
        our_retry(self.get_retries(), || {
            self.update_last_active_at_retriable(ingest_invocation_id)
        })
    }

    /// Get any ETxOutRecords produced by a given ingress key for a given
    /// block index.
    ///
    /// Arguments:
    /// * ingress_key: The ingress key we need ETxOutRecords from
    /// * block_index: The block we need ETxOutRecords from
    ///
    /// Returns:
    /// * The ETxOutRecord's from when this block was added, or None if the
    ///   block doesn't exist yet or, an error
    fn get_tx_outs_by_block_and_key(
        &self,
        ingress_key: CompressedRistrettoPublic,
        block_index: u64,
    ) -> Result<Option<Vec<ETxOutRecord>>, Self::Error> {
        our_retry(self.get_retries(), || {
            self.get_tx_outs_by_block_and_key_retriable(ingress_key, block_index)
        })
    }

    /// Get ETxOutRecords for a given ingress key from a block, and subsequent
    /// blocks, up to some limit. (This is like a batch call to
    /// get_tx_outs_by_block_and_key with lookahead, and makes sense if
    /// there is high network latency.)
    ///
    /// Arguments:
    /// * ingress_key: The ingress key we need ETxOutRecords from
    /// * block_range: The range of blocks to get ETxOutRecords from.
    ///
    /// Returns:
    /// * The sequence of ETxOutRecord's, from consecutive blocks starting from
    ///   block_index. Empty if not even the block_index'th block exists.
    fn get_tx_outs_by_block_range_and_key(
        &self,
        ingress_key: CompressedRistrettoPublic,
        block_range: &BlockRange,
    ) -> Result<Vec<Vec<ETxOutRecord>>, Self::Error> {
        our_retry(self.get_retries(), || {
            self.get_tx_outs_by_block_range_and_key_retriable(ingress_key, block_range)
        })
    }

    /// Get iid that produced data for given ingress key and a given block
    /// index.
    fn get_invocation_id_by_block_and_key(
        &self,
        ingress_key: CompressedRistrettoPublic,
        block_index: u64,
    ) -> Result<Option<IngestInvocationId>, Self::Error> {
        our_retry(self.get_retries(), || {
            self.get_invocation_id_by_block_and_key_retriable(ingress_key, block_index)
        })
    }

    /// Get the cumulative txo count for a given block number.
    ///
    /// Arguments:
    /// * block_index: The block we need cumulative_txo_count for.
    ///
    /// Returns:
    /// * Some(cumulative_txo_count) if the block was found in the database,
    ///   None if it wasn't, or an error if the query failed.
    fn get_cumulative_txo_count_for_block(
        &self,
        block_index: u64,
    ) -> Result<Option<u64>, Self::Error> {
        our_retry(self.get_retries(), || {
            self.get_cumulative_txo_count_for_block_retriable(block_index)
        })
    }

    /// Get the block signature timestamp for a given block number.
    /// Note that it is unspecified which timestamp we use if there are multiple
    /// timestamps.
    ///
    /// Arguments:
    /// * block_index: The block we need timestamp for.
    ///
    /// Returns:
    /// * Some(cumulative_txo_count) if the block was found in the database,
    ///   None if it wasn't, or an error if the query failed.
    fn get_block_signature_timestamp_for_block(
        &self,
        block_index: u64,
    ) -> Result<Option<u64>, Self::Error> {
        our_retry(self.get_retries(), || {
            self.get_block_signature_timestamp_for_block_retriable(block_index)
        })
    }

    /// Get the highest block index for which we have any data at all.
    fn get_highest_known_block_index(&self) -> Result<Option<u64>, Self::Error> {
        our_retry(self.get_retries(), || {
            self.get_highest_known_block_index_retriable()
        })
    }

    /// Returns all ingest invocations have not been active since the
    /// `expiration`.
    fn get_expired_invocations(
        &self,
        expiration: NaiveDateTime,
    ) -> Result<Vec<ExpiredInvocationRecord>, Error> {
        our_retry(self.get_retries(), || {
            self.get_expired_invocations_retriable(expiration)
        })
    }
}

/// See trait `fog_recovery_db_iface::ReportDb` for documentation.
impl ReportDb for SqlRecoveryDb {
    type Error = Error;

    fn get_all_reports(&self) -> Result<Vec<(String, ReportData)>, Self::Error> {
        our_retry(self.get_retries(), || self.get_all_reports_retriable())
    }

    /// Set report data associated with a given report id.
    fn set_report(
        &self,
        ingress_key: &CompressedRistrettoPublic,
        report_id: &str,
        data: &ReportData,
    ) -> Result<IngressPublicKeyStatus, Self::Error> {
        our_retry(self.get_retries(), || {
            self.set_report_retriable(ingress_key, report_id, data)
        })
    }

    /// Remove report data associated with a given report id.
    fn remove_report(&self, report_id: &str) -> Result<(), Self::Error> {
        our_retry(self.get_retries(), || {
            self.remove_report_retriable(report_id)
        })
    }
}

// Helper for using the retry crate's retry function
//
// The retry crate has From<Result<R, E>> for OperationResult, but this does
// not check if E is retriable, it just always assumes it is.
// https://docs.rs/retry/latest/src/retry/opresult.rs.html#32-39
//
// The retry::retry function will implicitly use this conversion unless you
// explicitly map things to OperationResult, which is kind of a footgun.
//
// This version only works with our Error, but actually tests if it is retriable
//
// We also unpack the RetryError object which has a useless variant
//
// Note: We may want to move this to a util crate or something, but if we do
// then there would need to be a common trait for "should_retry()" errors.
fn our_retry<I, O, R>(iterable: I, mut operation: O) -> Result<R, Error>
where
    I: IntoIterator<Item = Duration>,
    O: FnMut() -> Result<R, Error>,
{
    retry::retry(iterable, || match operation() {
        Ok(ok) => OperationResult::Ok(ok),
        Err(err) => {
            if err.should_retry() {
                OperationResult::Retry(err)
            } else {
                OperationResult::Err(err)
            }
        }
    })
    .map_err(unpack_retry_error)
}

fn unpack_retry_error(src: RetryError<Error>) -> Error {
    src.error
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::prelude::*;
    use mc_attest_verifier_types::prost;
    use mc_common::{logger::test_with_logger, HashSet};
    use mc_crypto_keys::RistrettoPublic;
    use mc_fog_report_types::AttestationEvidence;
    use mc_fog_test_infra::db_tests::{random_block, random_kex_rng_pubkey};
    use mc_util_from_random::FromRandom;
    use rand::{rngs::StdRng, thread_rng, SeedableRng};

    #[test_with_logger]
    fn test_new_ingest_invocation(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger.clone());
        let db = db_test_context.get_db_instance();

        let ingress_key1 = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key1, 0).unwrap();

        let egress_key1 = random_kex_rng_pubkey(&mut rng);
        let invoc_id1 = db
            .new_ingest_invocation(None, &ingress_key1, &egress_key1, 0)
            .unwrap();
        log::info!(logger, "first invoc id: {}", invoc_id1);

        let ingress_key2 = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key2, 100).unwrap();

        let egress_key2 = random_kex_rng_pubkey(&mut rng);
        let invoc_id2 = db
            .new_ingest_invocation(None, &ingress_key2, &egress_key2, 100)
            .unwrap();
        log::info!(logger, "second invoc id: {}", invoc_id2);

        assert_ne!(invoc_id1, invoc_id2);

        // Both ingest invocations should appear in the ingest_invocations table
        let conn = &mut db_test_context.new_conn();
        let ingest_invocations: Vec<models::IngestInvocation> =
            schema::ingest_invocations::dsl::ingest_invocations
                .order_by(schema::ingest_invocations::dsl::id)
                .load(conn)
                .expect("failed getting ingest invocations");

        assert_eq!(ingest_invocations.len(), 2);

        assert_eq!(
            IngestInvocationId::from(ingest_invocations[0].id),
            invoc_id1
        );
        assert_eq!(*ingest_invocations[0].ingress_public_key, ingress_key1);
        assert_eq!(
            ingest_invocations[0].egress_public_key,
            egress_key1.public_key
        );
        assert_eq!(
            ingest_invocations[0].rng_version as u32,
            egress_key1.version
        );
        assert_eq!(ingest_invocations[0].start_block, 0);
        assert!(!ingest_invocations[0].decommissioned);

        assert_eq!(
            IngestInvocationId::from(ingest_invocations[1].id),
            invoc_id2
        );
        assert_eq!(*ingest_invocations[1].ingress_public_key, ingress_key2);
        assert_eq!(
            ingest_invocations[1].egress_public_key,
            egress_key2.public_key
        );
        assert_eq!(
            ingest_invocations[1].rng_version as u32,
            egress_key2.version
        );
        assert_eq!(ingest_invocations[1].start_block, 100);
        assert!(!ingest_invocations[1].decommissioned);
    }

    #[test_with_logger]
    fn test_get_ingestable_ranges(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        // Should return an empty array when we have no invocations.
        let ranges = db.get_ingestable_ranges().unwrap();
        assert!(ranges.is_empty());

        let ingress_key = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key, 123).unwrap();

        // Add an ingest invocation and see that we can see it.
        let invoc_id1 = db
            .new_ingest_invocation(None, &ingress_key, &random_kex_rng_pubkey(&mut rng), 123)
            .unwrap();

        let ranges = db.get_ingestable_ranges().unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].id, invoc_id1);
        assert_eq!(ranges[0].start_block, 123);
        assert!(!ranges[0].decommissioned);
        assert_eq!(ranges[0].last_ingested_block, None);

        // Add an ingested block and see that last_ingested_block gets updated.
        for block_index in 123..130 {
            let (block, records) = random_block(&mut rng, block_index, 10);

            db.add_block_data(&invoc_id1, &block, 0, &records).unwrap();

            let ranges = db.get_ingestable_ranges().unwrap();
            assert_eq!(ranges.len(), 1);
            assert_eq!(ranges[0].id, invoc_id1);
            assert_eq!(ranges[0].start_block, 123);
            assert!(!ranges[0].decommissioned);
            assert_eq!(ranges[0].last_ingested_block, Some(block_index));
        }

        // Add another ingest invocation and see we get the expected data.
        let invoc_id2 = db
            .new_ingest_invocation(None, &ingress_key, &random_kex_rng_pubkey(&mut rng), 1020)
            .unwrap();

        let ranges = db.get_ingestable_ranges().unwrap();
        assert_eq!(ranges.len(), 2);

        assert_eq!(ranges[0].id, invoc_id1);
        assert_eq!(ranges[0].start_block, 123);
        assert!(!ranges[0].decommissioned);
        assert_eq!(ranges[0].last_ingested_block, Some(129));

        assert_eq!(ranges[1].id, invoc_id2);
        assert_eq!(ranges[1].start_block, 1020);
        assert!(!ranges[1].decommissioned);
        assert_eq!(ranges[1].last_ingested_block, None);

        // Decomission the first ingest invocation and validate the returned data.
        db.decommission_ingest_invocation(&invoc_id1).unwrap();

        let ranges = db.get_ingestable_ranges().unwrap();
        assert_eq!(ranges.len(), 2);

        assert_eq!(ranges[0].id, invoc_id1);
        assert_eq!(ranges[0].start_block, 123);
        assert!(ranges[0].decommissioned);
        assert_eq!(ranges[0].last_ingested_block, Some(129));

        assert_eq!(ranges[1].id, invoc_id2);
        assert_eq!(ranges[1].start_block, 1020);
        assert!(!ranges[1].decommissioned);
        assert_eq!(ranges[1].last_ingested_block, None);

        // Decomission the second ingest invocation and validate the returned data.
        db.decommission_ingest_invocation(&invoc_id2).unwrap();

        let ranges = db.get_ingestable_ranges().unwrap();
        assert_eq!(ranges.len(), 2);

        assert_eq!(ranges[0].id, invoc_id1);
        assert_eq!(ranges[0].start_block, 123);
        assert!(ranges[0].decommissioned);
        assert_eq!(ranges[0].last_ingested_block, Some(129));

        assert_eq!(ranges[1].id, invoc_id2);
        assert_eq!(ranges[1].start_block, 1020);
        assert!(ranges[1].decommissioned);
        assert_eq!(ranges[1].last_ingested_block, None);
    }

    #[test_with_logger]
    fn test_decommission_ingest_invocation(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        let ingress_key = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key, 123).unwrap();

        let invoc_id1 = db
            .new_ingest_invocation(None, &ingress_key, &random_kex_rng_pubkey(&mut rng), 123)
            .unwrap();

        let invoc_id2 = db
            .new_ingest_invocation(None, &ingress_key, &random_kex_rng_pubkey(&mut rng), 456)
            .unwrap();

        // Initially both ingest invocations should not be decommissioned.
        let ranges = db.get_ingestable_ranges().unwrap();
        assert_eq!(ranges.len(), 2);

        assert_eq!(ranges[0].id, invoc_id1);
        assert_eq!(ranges[0].start_block, 123);
        assert!(!ranges[0].decommissioned);
        assert_eq!(ranges[0].last_ingested_block, None);

        assert_eq!(ranges[1].id, invoc_id2);
        assert_eq!(ranges[1].start_block, 456);
        assert!(!ranges[1].decommissioned);
        assert_eq!(ranges[1].last_ingested_block, None);

        // Ensure we do not have any decommissioning events.
        let (events, next_start_from_user_event_id) = db.search_user_events(0).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, FogUserEvent::DecommissionIngestInvocation(_)))
                .count(),
            0
        );

        // Decommission the 2nd ingest invocation and test again
        db.decommission_ingest_invocation(&invoc_id2).unwrap();

        let ranges = db.get_ingestable_ranges().unwrap();
        assert_eq!(ranges.len(), 2);

        assert_eq!(ranges[0].id, invoc_id1);
        assert_eq!(ranges[0].start_block, 123);
        assert!(!ranges[0].decommissioned);
        assert_eq!(ranges[0].last_ingested_block, None);

        assert_eq!(ranges[1].id, invoc_id2);
        assert_eq!(ranges[1].start_block, 456);
        assert!(ranges[1].decommissioned);
        assert_eq!(ranges[1].last_ingested_block, None);

        // We should have one decommissioning event.
        let (events, next_start_from_user_event_id) = db
            .search_user_events(next_start_from_user_event_id)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            FogUserEvent::DecommissionIngestInvocation(
                mc_fog_types::view::DecommissionedIngestInvocation {
                    ingest_invocation_id: *ranges[1].id,
                    last_ingested_block: 0,
                },
            ),
        );

        // Decommission an invalid ingest invocation id
        let result = db.decommission_ingest_invocation(&IngestInvocationId::from(123));
        assert!(result.is_err());

        // Decommission the 1st ingest invocation by creating a third invocation.
        let invoc_id3_kex_rng_pubkey = random_kex_rng_pubkey(&mut rng);
        let invoc_id3 = db
            .new_ingest_invocation(
                Some(invoc_id1),
                &ingress_key,
                &invoc_id3_kex_rng_pubkey,
                456,
            )
            .unwrap();

        let ranges = db.get_ingestable_ranges().unwrap();
        assert_eq!(ranges.len(), 3);

        assert_eq!(ranges[0].id, invoc_id1);
        assert_eq!(ranges[0].start_block, 123);
        assert!(ranges[0].decommissioned);
        assert_eq!(ranges[0].last_ingested_block, None);

        assert_eq!(ranges[1].id, invoc_id2);
        assert_eq!(ranges[1].start_block, 456);
        assert!(ranges[1].decommissioned);
        assert_eq!(ranges[1].last_ingested_block, None);

        assert_eq!(ranges[2].id, invoc_id3);
        assert_eq!(ranges[2].start_block, 456);
        assert!(!ranges[2].decommissioned);
        assert_eq!(ranges[2].last_ingested_block, None);

        // We should have one decommissioning event and one new ingest invocation event.
        let (events, _next_start_from_user_event_id) = db
            .search_user_events(next_start_from_user_event_id)
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            FogUserEvent::DecommissionIngestInvocation(
                mc_fog_types::view::DecommissionedIngestInvocation {
                    ingest_invocation_id: *ranges[0].id,
                    last_ingested_block: 0,
                },
            ),
        );
        assert_eq!(
            events[1],
            FogUserEvent::NewRngRecord(mc_fog_types::view::RngRecord {
                ingest_invocation_id: *invoc_id3,
                pubkey: invoc_id3_kex_rng_pubkey,
                start_block: 456,
            })
        );
    }

    #[test_with_logger]
    fn test_add_block_data(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();
        let conn = &mut db_test_context.new_conn();

        let ingress_key = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key, 20).unwrap();

        let invoc_id1 = db
            .new_ingest_invocation(None, &ingress_key, &random_kex_rng_pubkey(&mut rng), 10)
            .unwrap();

        let invoc_id2 = db
            .new_ingest_invocation(None, &ingress_key, &random_kex_rng_pubkey(&mut rng), 15)
            .unwrap();

        let (block1, mut records1) = random_block(&mut rng, 20, 10);
        records1.sort_by_key(|rec| rec.search_key.clone()); // this makes comparing tests result predictable.

        let (block2, mut records2) = random_block(&mut rng, 21, 15);
        records2.sort_by_key(|rec| rec.search_key.clone()); // this makes comparing tests result predictable.

        // Get the last_active_at of the two ingest invocations so we could compare to
        // it later.
        let invocs_last_active_at: Vec<chrono::NaiveDateTime> =
            schema::ingest_invocations::dsl::ingest_invocations
                .select(schema::ingest_invocations::dsl::last_active_at)
                .order_by(schema::ingest_invocations::dsl::id)
                .load(conn)
                .unwrap();
        let mut invoc1_orig_last_active_at = invocs_last_active_at[0];
        let invoc2_orig_last_active_at = invocs_last_active_at[1];

        // Sleep a second so that the timestamp update would show if it happens.
        std::thread::sleep(std::time::Duration::from_secs(1));

        // Add the block data to the first invocation and test that everything got
        // written correctly.
        db.add_block_data(&invoc_id1, &block1, 0, &records1)
            .unwrap();

        let blocks: Vec<models::IngestedBlock> = schema::ingested_blocks::dsl::ingested_blocks
            .order_by(schema::ingested_blocks::dsl::id)
            .load(conn)
            .unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            IngestInvocationId::from(blocks[0].ingest_invocation_id),
            invoc_id1
        );
        assert_eq!(blocks[0].block_number as u64, block1.index);
        assert_eq!(
            blocks[0].cumulative_txo_count as u64,
            block1.cumulative_txo_count
        );

        let e_tx_out_records = db
            .get_tx_outs_by_block_and_key(ingress_key, block1.index)
            .unwrap()
            .unwrap();
        assert_eq!(e_tx_out_records.len(), 10);
        assert_eq!(e_tx_out_records.len(), records1.len());
        for (expected_record, written_record) in records1.iter().zip(e_tx_out_records.iter()) {
            assert_eq!(written_record.search_key, expected_record.search_key);
            assert_eq!(written_record.payload, expected_record.payload);
        }

        // Last active at of invoc1 should've updated
        let invocs_last_active_at: Vec<chrono::NaiveDateTime> =
            schema::ingest_invocations::dsl::ingest_invocations
                .select(schema::ingest_invocations::dsl::last_active_at)
                .order_by(schema::ingest_invocations::dsl::id)
                .load(conn)
                .unwrap();
        assert!(invocs_last_active_at[0] > invoc1_orig_last_active_at);
        assert_eq!(invocs_last_active_at[1], invoc2_orig_last_active_at);

        invoc1_orig_last_active_at = invocs_last_active_at[0];

        // Sleep so that timestamp change is noticeable if it happens
        std::thread::sleep(std::time::Duration::from_secs(1));

        // Adding the same block again should fail.
        assert_eq!(
            db.add_block_data(&invoc_id1, &block1, 0, &records1)
                .unwrap(),
            AddBlockDataStatus {
                block_already_scanned_with_this_key: true
            }
        );
        assert_eq!(
            db.add_block_data(&invoc_id1, &block1, 0, &records2)
                .unwrap(),
            AddBlockDataStatus {
                block_already_scanned_with_this_key: true
            }
        );

        // Timestamps should not change.
        let invocs_last_active_at: Vec<chrono::NaiveDateTime> =
            schema::ingest_invocations::dsl::ingest_invocations
                .select(schema::ingest_invocations::dsl::last_active_at)
                .order_by(schema::ingest_invocations::dsl::id)
                .load(conn)
                .unwrap();
        assert_eq!(invocs_last_active_at[0], invoc1_orig_last_active_at);
        assert_eq!(invocs_last_active_at[1], invoc2_orig_last_active_at);

        // Add a different block to the 2nd ingest invocation.
        db.add_block_data(&invoc_id2, &block2, 0, &records2)
            .unwrap();
        assert_eq!(
            db.add_block_data(&invoc_id2, &block2, 0, &records1)
                .unwrap(),
            AddBlockDataStatus {
                block_already_scanned_with_this_key: true
            }
        );

        let blocks: Vec<models::IngestedBlock> = schema::ingested_blocks::dsl::ingested_blocks
            .order_by(schema::ingested_blocks::dsl::id)
            .load(conn)
            .unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            IngestInvocationId::from(blocks[0].ingest_invocation_id),
            invoc_id1
        );
        assert_eq!(blocks[0].block_number as u64, block1.index);
        assert_eq!(
            blocks[0].cumulative_txo_count as u64,
            block1.cumulative_txo_count
        );

        assert_eq!(
            IngestInvocationId::from(blocks[1].ingest_invocation_id),
            invoc_id2
        );
        assert_eq!(blocks[1].block_number as u64, block2.index);
        assert_eq!(
            blocks[1].cumulative_txo_count as u64,
            block2.cumulative_txo_count
        );

        let mut e_tx_out_records = db
            .get_tx_outs_by_block_and_key(ingress_key, block1.index)
            .unwrap()
            .unwrap();
        let mut e_tx_out_records_b1 = db
            .get_tx_outs_by_block_and_key(ingress_key, block2.index)
            .unwrap()
            .unwrap();
        e_tx_out_records.append(&mut e_tx_out_records_b1);
        assert_eq!(e_tx_out_records.len(), 25);
        assert_eq!(e_tx_out_records.len(), records1.len() + records2.len());

        // Last active at of invoc2 should've updated
        let invocs_last_active_at: Vec<chrono::NaiveDateTime> =
            schema::ingest_invocations::dsl::ingest_invocations
                .select(schema::ingest_invocations::dsl::last_active_at)
                .order_by(schema::ingest_invocations::dsl::id)
                .load(conn)
                .unwrap();
        assert_eq!(invocs_last_active_at[0], invoc1_orig_last_active_at);
        assert!(invocs_last_active_at[1] > invoc2_orig_last_active_at);
    }

    #[test_with_logger]
    fn test_search_user_events(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        let ingress_key = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key, 123).unwrap();

        // Create 3 new ingest invocations.
        let kex_rng_pubkeys: Vec<KexRngPubkey> =
            (0..3).map(|_| random_kex_rng_pubkey(&mut rng)).collect();

        let invoc_ids: Vec<_> = kex_rng_pubkeys
            .iter()
            .map(|kex_rng_pubkey| {
                db.new_ingest_invocation(None, &ingress_key, kex_rng_pubkey, 123)
                    .unwrap()
            })
            .collect();

        // Add a decomission record
        db.decommission_ingest_invocation(&invoc_ids[1]).unwrap();

        // Add two missing block records.
        let ingress_key1 = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key1, 10).unwrap();
        db.set_report(
            &ingress_key1,
            "",
            &ReportData {
                pubkey_expiry: 20,
                ingest_invocation_id: None,
                attestation_evidence: create_attestation_evidence(""),
            },
        )
        .unwrap();
        db.report_lost_ingress_key(ingress_key1).unwrap();

        let ingress_key2 = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key2, 30).unwrap();
        db.set_report(
            &ingress_key2,
            "",
            &ReportData {
                pubkey_expiry: 40,
                ingest_invocation_id: None,
                attestation_evidence: create_attestation_evidence(""),
            },
        )
        .unwrap();
        db.report_lost_ingress_key(ingress_key2).unwrap();

        // Search for events and verify the results.
        let (events, _) = db.search_user_events(0).unwrap();
        assert_eq!(
            events,
            vec![
                FogUserEvent::NewRngRecord(mc_fog_types::view::RngRecord {
                    ingest_invocation_id: *invoc_ids[0],
                    pubkey: kex_rng_pubkeys[0].clone(),
                    start_block: 123,
                }),
                FogUserEvent::NewRngRecord(mc_fog_types::view::RngRecord {
                    ingest_invocation_id: *invoc_ids[1],
                    pubkey: kex_rng_pubkeys[1].clone(),
                    start_block: 123,
                }),
                FogUserEvent::NewRngRecord(mc_fog_types::view::RngRecord {
                    ingest_invocation_id: *invoc_ids[2],
                    pubkey: kex_rng_pubkeys[2].clone(),
                    start_block: 123,
                }),
                FogUserEvent::DecommissionIngestInvocation(
                    mc_fog_types::view::DecommissionedIngestInvocation {
                        ingest_invocation_id: *invoc_ids[1],
                        last_ingested_block: 0
                    }
                ),
                FogUserEvent::MissingBlocks(mc_fog_types::common::BlockRange {
                    start_block: 10,
                    end_block: 20
                }),
                FogUserEvent::MissingBlocks(mc_fog_types::common::BlockRange {
                    start_block: 30,
                    end_block: 40
                })
            ]
        );

        // Searching with a start_from_user_id that is higher than the highest available
        // one should return nothing.
        let (_events, next_start_from_user_event_id) = db.search_user_events(0).unwrap();

        let (events, next_start_from_user_event_id2) = db
            .search_user_events(next_start_from_user_event_id)
            .unwrap();
        assert_eq!(events.len(), 0);
        assert_eq!(
            next_start_from_user_event_id,
            next_start_from_user_event_id2
        );

        let (events, next_start_from_user_event_id2) = db
            .search_user_events(next_start_from_user_event_id + 1)
            .unwrap();
        assert_eq!(events.len(), 0);
        assert_eq!(
            next_start_from_user_event_id + 1,
            next_start_from_user_event_id2,
            "Expected to recieve next_start_from_user_event_id equal to query when no new values are found: {} != {}", next_start_from_user_event_id + 1, next_start_from_user_event_id2,
        );
    }

    #[test_with_logger]
    fn test_get_tx_outs(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        let ingress_key = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key, 123).unwrap();

        let invoc_id = db
            .new_ingest_invocation(None, &ingress_key, &random_kex_rng_pubkey(&mut rng), 123)
            .unwrap();

        let first_block_index = 10;

        let (block1, records1) = random_block(&mut rng, first_block_index, 10);
        db.add_block_data(&invoc_id, &block1, 0, &records1).unwrap();

        let (block2, records2) = random_block(&mut rng, first_block_index + 1, 10);
        db.add_block_data(&invoc_id, &block2, 0, &records2).unwrap();

        // Search for non-existent keys, all should be NotFound
        for test_case in &[
            vec![],
            vec![vec![]],
            vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8]],
            vec![[1; 32].to_vec()],
        ] {
            let results = db.get_tx_outs(0, test_case).unwrap();
            assert_eq!(
                results,
                test_case
                    .iter()
                    .map(|search_key| FixedTxOutSearchResult::new_not_found(search_key.clone()))
                    .collect::<Vec<_>>()
            );
        }

        // Search for some non-existent keys and some that we expect to find.
        let test_case = vec![
            vec![1, 2, 3, 4],
            records1[0].search_key.clone(),
            records1[5].search_key.clone(),
            records2[3].search_key.clone(),
            vec![5, 6, 7, 8],
        ];
        let results = db.get_tx_outs(0, &test_case).unwrap();
        assert_eq!(
            results,
            vec![
                FixedTxOutSearchResult::new_not_found(test_case[0].clone()),
                FixedTxOutSearchResult::new(
                    test_case[1].clone(),
                    &records1[0].payload,
                    TxOutSearchResultCode::Found
                ),
                FixedTxOutSearchResult::new(
                    test_case[2].clone(),
                    &records1[5].payload,
                    TxOutSearchResultCode::Found
                ),
                FixedTxOutSearchResult::new(
                    test_case[3].clone(),
                    &records2[3].payload,
                    TxOutSearchResultCode::Found
                ),
                FixedTxOutSearchResult::new_not_found(test_case[4].clone()),
            ]
        );

        let results = db.get_tx_outs(first_block_index, &test_case).unwrap();
        assert_eq!(
            results,
            vec![
                FixedTxOutSearchResult::new_not_found(test_case[0].clone()),
                FixedTxOutSearchResult::new(
                    test_case[1].clone(),
                    &records1[0].payload,
                    TxOutSearchResultCode::Found
                ),
                FixedTxOutSearchResult::new(
                    test_case[2].clone(),
                    &records1[5].payload,
                    TxOutSearchResultCode::Found
                ),
                FixedTxOutSearchResult::new(
                    test_case[3].clone(),
                    &records2[3].payload,
                    TxOutSearchResultCode::Found
                ),
                FixedTxOutSearchResult::new_not_found(test_case[4].clone()),
            ]
        );

        // Searching with a start_block that filters out the results should filter them
        // as expected.
        let results = db.get_tx_outs(first_block_index + 5, &test_case).unwrap();
        assert_eq!(
            results,
            vec![
                FixedTxOutSearchResult::new_not_found(test_case[0].clone()),
                FixedTxOutSearchResult::new_not_found(test_case[1].clone()),
                FixedTxOutSearchResult::new_not_found(test_case[2].clone()),
                FixedTxOutSearchResult::new_not_found(test_case[3].clone()),
                FixedTxOutSearchResult::new_not_found(test_case[4].clone()),
            ]
        );

        let results = db.get_tx_outs(first_block_index + 1, &test_case).unwrap();
        assert_eq!(
            results,
            vec![
                FixedTxOutSearchResult::new_not_found(test_case[0].clone()),
                FixedTxOutSearchResult::new_not_found(test_case[1].clone()),
                FixedTxOutSearchResult::new_not_found(test_case[2].clone()),
                FixedTxOutSearchResult::new(
                    test_case[3].clone(),
                    &records2[3].payload,
                    TxOutSearchResultCode::Found
                ),
                FixedTxOutSearchResult::new_not_found(test_case[4].clone()),
            ]
        );
    }

    #[test_with_logger]
    fn test_get_tx_outs_by_block_and_key(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        let ingress_key = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key, 122).unwrap();

        let invoc_id1 = db
            .new_ingest_invocation(None, &ingress_key, &random_kex_rng_pubkey(&mut rng), 122)
            .unwrap();

        let invoc_id2 = db
            .new_ingest_invocation(None, &ingress_key, &random_kex_rng_pubkey(&mut rng), 123)
            .unwrap();

        let (block1, records1) = random_block(&mut rng, 122, 10);
        db.add_block_data(&invoc_id1, &block1, 0, &records1)
            .unwrap();

        let (block2, records2) = random_block(&mut rng, 123, 10);
        db.add_block_data(&invoc_id2, &block2, 0, &records2)
            .unwrap();

        // Get tx outs for a key we're not aware of or a block id we're not aware of
        // should return None
        let tx_outs = db.get_tx_outs_by_block_and_key(ingress_key, 124).unwrap();
        assert_eq!(tx_outs, None);

        let tx_outs = db
            .get_tx_outs_by_block_and_key(CompressedRistrettoPublic::from_random(&mut rng), 123)
            .unwrap();
        assert_eq!(tx_outs, None);

        // Getting tx outs for ingress key and block number that were previously written
        // should work as expected.
        let tx_outs = db
            .get_tx_outs_by_block_and_key(ingress_key, block1.index)
            .unwrap()
            .unwrap();
        assert_eq!(tx_outs, records1);

        let tx_outs = db
            .get_tx_outs_by_block_and_key(ingress_key, block2.index)
            .unwrap()
            .unwrap();
        assert_eq!(tx_outs, records2);
    }

    #[test_with_logger]
    fn test_get_tx_outs_by_block_range_and_key(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        let ingress_key = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key, 122).unwrap();

        let invoc_id1 = db
            .new_ingest_invocation(None, &ingress_key, &random_kex_rng_pubkey(&mut rng), 122)
            .unwrap();

        let invoc_id2 = db
            .new_ingest_invocation(None, &ingress_key, &random_kex_rng_pubkey(&mut rng), 123)
            .unwrap();

        let (block1, records1) = random_block(&mut rng, 122, 10);
        db.add_block_data(&invoc_id1, &block1, 0, &records1)
            .unwrap();

        let (block2, records2) = random_block(&mut rng, 123, 10);
        db.add_block_data(&invoc_id2, &block2, 0, &records2)
            .unwrap();

        // Get tx outs for a key we're not aware of or a block id we're not aware of
        // should return empty vec
        let block_range = BlockRange::new_from_length(124, 2);
        let batch_result = db
            .get_tx_outs_by_block_range_and_key(ingress_key, &block_range)
            .unwrap();
        assert_eq!(batch_result.len(), 0);

        let block_range = BlockRange::new_from_length(123, 2);
        let batch_result = db
            .get_tx_outs_by_block_range_and_key(
                CompressedRistrettoPublic::from_random(&mut rng),
                &block_range,
            )
            .unwrap();
        assert_eq!(batch_result.len(), 0);

        // Getting tx outs in a batch should work as expected when requesting things
        // that exist
        let block_range = BlockRange::new_from_length(block1.index, 1);
        let batch_results = db
            .get_tx_outs_by_block_range_and_key(ingress_key, &block_range)
            .unwrap();
        assert_eq!(batch_results.len(), 1);
        assert_eq!(batch_results[0], records1);

        let block_range = BlockRange::new_from_length(block2.index, 1);
        let batch_results = db
            .get_tx_outs_by_block_range_and_key(ingress_key, &block_range)
            .unwrap();
        assert_eq!(batch_results.len(), 1);
        assert_eq!(batch_results[0], records2);

        let block_range = BlockRange::new_from_length(block1.index, 2);
        let batch_results = db
            .get_tx_outs_by_block_range_and_key(ingress_key, &block_range)
            .unwrap();
        assert_eq!(batch_results.len(), 2);
        assert_eq!(batch_results[0], records1);
        assert_eq!(batch_results[1], records2);

        let block_range = BlockRange::new_from_length(block2.index, 2);
        let batch_results = db
            .get_tx_outs_by_block_range_and_key(ingress_key, &block_range)
            .unwrap();
        assert_eq!(batch_results.len(), 1);
        assert_eq!(batch_results[0], records2);

        let block_range = BlockRange::new_from_length(block1.index, 3);
        let batch_results = db
            .get_tx_outs_by_block_range_and_key(ingress_key, &block_range)
            .unwrap();
        assert_eq!(batch_results.len(), 2);
        assert_eq!(batch_results[0], records1);
        assert_eq!(batch_results[1], records2);

        let block_range = BlockRange::new_from_length(block2.index, 3);
        let batch_results = db
            .get_tx_outs_by_block_range_and_key(ingress_key, &block_range)
            .unwrap();
        assert_eq!(batch_results.len(), 1);
        assert_eq!(batch_results[0], records2);

        // When there is a gap in the data, the gap should suppress any further results
        // even if there are hits later in the range.
        let block_range = BlockRange::new_from_length(block1.index - 1, 2);
        let batch_results = db
            .get_tx_outs_by_block_range_and_key(ingress_key, &block_range)
            .unwrap();
        assert_eq!(batch_results.len(), 0);

        let block_range = BlockRange::new_from_length(block1.index - 2, 3);
        let batch_results = db
            .get_tx_outs_by_block_range_and_key(ingress_key, &block_range)
            .unwrap();
        assert_eq!(batch_results.len(), 0);
    }

    #[test_with_logger]
    fn test_get_highest_block_index(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        let ingress_key = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key, 120).unwrap();

        let invoc_id1 = db
            .new_ingest_invocation(None, &ingress_key, &random_kex_rng_pubkey(&mut rng), 120)
            .unwrap();

        let invoc_id2 = db
            .new_ingest_invocation(None, &ingress_key, &random_kex_rng_pubkey(&mut rng), 120)
            .unwrap();

        assert_eq!(db.get_highest_known_block_index().unwrap(), None);

        let (block, records) = random_block(&mut rng, 123, 10);
        db.add_block_data(&invoc_id1, &block, 0, &records).unwrap();

        assert_eq!(db.get_highest_known_block_index().unwrap(), Some(123));

        let (block, records) = random_block(&mut rng, 122, 10);
        db.add_block_data(&invoc_id2, &block, 0, &records).unwrap();

        assert_eq!(db.get_highest_known_block_index().unwrap(), Some(123));

        let (block, records) = random_block(&mut rng, 125, 10);
        db.add_block_data(&invoc_id2, &block, 0, &records).unwrap();

        assert_eq!(db.get_highest_known_block_index().unwrap(), Some(125));

        let (block, records) = random_block(&mut rng, 120, 10);
        db.add_block_data(&invoc_id2, &block, 0, &records).unwrap();

        assert_eq!(db.get_highest_known_block_index().unwrap(), Some(125));
    }

    fn create_attestation_evidence(name: &str) -> AttestationEvidence {
        let report_data = prost::EnclaveReportDataContents {
            nonce: format!("{name} nonce").into_bytes(),
            key: format!("{name} key").into_bytes(),
            custom_identity: format!("{name} custom_identity").into_bytes(),
        };
        prost::DcapEvidence {
            quote: None,
            collateral: None,
            report_data: Some(report_data),
        }
        .into()
    }

    #[test_with_logger]
    fn test_reports_db(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        let ingress_key = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key, 123).unwrap();

        let invoc_id1 = db
            .new_ingest_invocation(None, &ingress_key, &random_kex_rng_pubkey(&mut rng), 123)
            .unwrap();

        let invoc_id2 = db
            .new_ingest_invocation(None, &ingress_key, &random_kex_rng_pubkey(&mut rng), 123)
            .unwrap();

        // We start with no reports.
        assert_eq!(db.get_all_reports().unwrap(), vec![]);

        // Insert a report and see that we can get it back.
        let report_id1 = "";
        let report1 = ReportData {
            ingest_invocation_id: Some(invoc_id1),
            attestation_evidence: create_attestation_evidence(report_id1),
            pubkey_expiry: 102030,
        };
        let key_status = db.set_report(&ingress_key, report_id1, &report1).unwrap();
        assert_eq!(key_status.pubkey_expiry, 102030);

        assert_eq!(
            db.get_all_reports().unwrap(),
            vec![(report_id1.into(), report1.clone())]
        );

        // Insert another report and see that we can get it back.
        let report_id2 = "report 2";
        let report2 = ReportData {
            ingest_invocation_id: Some(invoc_id2),
            attestation_evidence: create_attestation_evidence(report_id2),
            pubkey_expiry: 10203040,
        };
        let key_status = db.set_report(&ingress_key, report_id2, &report2).unwrap();
        assert_eq!(key_status.pubkey_expiry, 10203040);

        assert_eq!(
            db.get_all_reports().unwrap(),
            vec![
                (report_id1.into(), report1),
                (report_id2.into(), report2.clone()),
            ]
        );

        // Update an existing report.
        let updated_report1 = ReportData {
            ingest_invocation_id: Some(invoc_id2),
            attestation_evidence: create_attestation_evidence("updated_report1"),
            pubkey_expiry: 424242,
        };

        db.set_report(&ingress_key, report_id1, &updated_report1)
            .unwrap();
        assert_eq!(
            key_status.pubkey_expiry, 10203040,
            "pubkey expiry should not have decreased"
        );

        assert_eq!(
            db.get_all_reports().unwrap(),
            vec![
                (report_id1.into(), updated_report1),
                (report_id2.into(), report2.clone()),
            ]
        );

        // Delete the first report and ensure it got removed.
        db.remove_report(report_id1).unwrap();

        assert_eq!(
            db.get_all_reports().unwrap(),
            vec![(report_id2.into(), report2)]
        );

        // Retire the ingress public key
        db.retire_ingress_key(&ingress_key, true).unwrap();

        let report1 = ReportData {
            ingest_invocation_id: Some(invoc_id1),
            attestation_evidence: create_attestation_evidence(report_id1),
            pubkey_expiry: 10203050,
        };
        let key_status = db.set_report(&ingress_key, report_id1, &report1).unwrap();
        assert_eq!(
            key_status.pubkey_expiry, 10203040,
            "pubkey expiry should not have increased after retiring the key"
        );

        // Unretire the ingress public key
        db.retire_ingress_key(&ingress_key, false).unwrap();

        let report1 = ReportData {
            ingest_invocation_id: Some(invoc_id1),
            attestation_evidence: create_attestation_evidence(report_id1),
            pubkey_expiry: 10203060,
        };
        let key_status = db.set_report(&ingress_key, report_id1, &report1).unwrap();
        assert_eq!(
            key_status.pubkey_expiry, 10203060,
            "pubkey expiry should have increased again after unretiring the key"
        );
    }

    #[test_with_logger]
    fn test_get_ingress_key_records(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        // At first, there are no records.
        assert_eq!(
            db.get_ingress_key_records(
                0,
                &IngressPublicKeyRecordFilters {
                    should_include_lost_keys: true,
                    should_include_retired_keys: true,
                    should_only_include_unexpired_keys: false,
                }
            )
            .unwrap(),
            vec![],
        );

        // Add an ingress key and see that we can retreive it.
        let ingress_key1 = CompressedRistrettoPublic::from_random(&mut rng);
        db.new_ingress_key(&ingress_key1, 123).unwrap();

        assert_eq!(
            db.get_ingress_key_records(
                0,
                &IngressPublicKeyRecordFilters {
                    should_include_lost_keys: true,
                    should_include_retired_keys: true,
                    should_only_include_unexpired_keys: false,
                }
            )
            .unwrap(),
            vec![IngressPublicKeyRecord {
                key: ingress_key1,
                status: IngressPublicKeyStatus {
                    start_block: 123,
                    pubkey_expiry: 0,
                    retired: false,
                    lost: false,
                },
                last_scanned_block: None,
            }],
        );

        // Add another ingress key and check that we can find it as well.
        let ingress_key2 = CompressedRistrettoPublic::from_random(&mut rng);
        db.new_ingress_key(&ingress_key2, 456).unwrap();

        assert_eq!(
            HashSet::<IngressPublicKeyRecord>::from_iter(
                db.get_ingress_key_records(
                    0,
                    &IngressPublicKeyRecordFilters {
                        should_include_lost_keys: true,
                        should_include_retired_keys: true,
                        should_only_include_unexpired_keys: false,
                    }
                )
                .unwrap()
            ),
            HashSet::from_iter(vec![
                IngressPublicKeyRecord {
                    key: ingress_key1,
                    status: IngressPublicKeyStatus {
                        start_block: 123,
                        pubkey_expiry: 0,
                        retired: false,
                        lost: false,
                    },
                    last_scanned_block: None,
                },
                IngressPublicKeyRecord {
                    key: ingress_key2,
                    status: IngressPublicKeyStatus {
                        start_block: 456,
                        pubkey_expiry: 0,
                        retired: false,
                        lost: false,
                    },
                    last_scanned_block: None,
                }
            ])
        );

        // Publish a few blocks and check that last_scanned_block gets updated as
        // expected.
        let invoc_id1 = db
            .new_ingest_invocation(None, &ingress_key1, &random_kex_rng_pubkey(&mut rng), 123)
            .unwrap();

        for block_id in 123..=130 {
            let (block, records) = random_block(&mut rng, block_id, 10);
            db.add_block_data(&invoc_id1, &block, 0, &records).unwrap();

            assert_eq!(
                HashSet::<IngressPublicKeyRecord>::from_iter(
                    db.get_ingress_key_records(
                        0,
                        &IngressPublicKeyRecordFilters {
                            should_include_lost_keys: true,
                            should_include_retired_keys: true,
                            should_only_include_unexpired_keys: false,
                        }
                    )
                    .unwrap()
                ),
                HashSet::from_iter(vec![
                    IngressPublicKeyRecord {
                        key: ingress_key1,
                        status: IngressPublicKeyStatus {
                            start_block: 123,
                            pubkey_expiry: 0,
                            retired: false,
                            lost: false,
                        },
                        last_scanned_block: Some(block_id),
                    },
                    IngressPublicKeyRecord {
                        key: ingress_key2,
                        status: IngressPublicKeyStatus {
                            start_block: 456,
                            pubkey_expiry: 0,
                            retired: false,
                            lost: false,
                        },
                        last_scanned_block: None,
                    }
                ])
            );
        }

        // Publishing an old block should not afftect last_scanned_block.
        let (block, records) = random_block(&mut rng, 50, 10);
        db.add_block_data(&invoc_id1, &block, 0, &records).unwrap();

        assert_eq!(
            HashSet::<IngressPublicKeyRecord>::from_iter(
                db.get_ingress_key_records(
                    0,
                    &IngressPublicKeyRecordFilters {
                        should_include_lost_keys: true,
                        should_include_retired_keys: true,
                        should_only_include_unexpired_keys: false,
                    }
                )
                .unwrap()
            ),
            HashSet::from_iter(vec![
                IngressPublicKeyRecord {
                    key: ingress_key1,
                    status: IngressPublicKeyStatus {
                        start_block: 123,
                        pubkey_expiry: 0,
                        retired: false,
                        lost: false,
                    },
                    last_scanned_block: Some(130),
                },
                IngressPublicKeyRecord {
                    key: ingress_key2,
                    status: IngressPublicKeyStatus {
                        start_block: 456,
                        pubkey_expiry: 0,
                        retired: false,
                        lost: false,
                    },
                    last_scanned_block: None,
                }
            ])
        );

        // Check that retiring behaves as expected.
        db.retire_ingress_key(&ingress_key1, true).unwrap();

        assert_eq!(
            HashSet::<IngressPublicKeyRecord>::from_iter(
                db.get_ingress_key_records(
                    0,
                    &IngressPublicKeyRecordFilters {
                        should_include_lost_keys: true,
                        should_include_retired_keys: true,
                        should_only_include_unexpired_keys: false,
                    }
                )
                .unwrap()
            ),
            HashSet::from_iter(vec![
                IngressPublicKeyRecord {
                    key: ingress_key1,
                    status: IngressPublicKeyStatus {
                        start_block: 123,
                        pubkey_expiry: 0,
                        retired: true,
                        lost: false,
                    },
                    last_scanned_block: Some(130),
                },
                IngressPublicKeyRecord {
                    key: ingress_key2,
                    status: IngressPublicKeyStatus {
                        start_block: 456,
                        pubkey_expiry: 0,
                        retired: false,
                        lost: false,
                    },
                    last_scanned_block: None,
                }
            ])
        );

        // Check that pubkey expiry behaves as expected
        db.set_report(
            &ingress_key2,
            "",
            &ReportData {
                ingest_invocation_id: None,
                attestation_evidence: create_attestation_evidence(""),
                pubkey_expiry: 888,
            },
        )
        .unwrap();

        assert_eq!(
            HashSet::<IngressPublicKeyRecord>::from_iter(
                db.get_ingress_key_records(
                    0,
                    &IngressPublicKeyRecordFilters {
                        should_include_lost_keys: true,
                        should_include_retired_keys: true,
                        should_only_include_unexpired_keys: false,
                    }
                )
                .unwrap()
            ),
            HashSet::from_iter(vec![
                IngressPublicKeyRecord {
                    key: ingress_key1,
                    status: IngressPublicKeyStatus {
                        start_block: 123,
                        pubkey_expiry: 0,
                        retired: true,
                        lost: false,
                    },
                    last_scanned_block: Some(130),
                },
                IngressPublicKeyRecord {
                    key: ingress_key2,
                    status: IngressPublicKeyStatus {
                        start_block: 456,
                        pubkey_expiry: 888,
                        retired: false,
                        lost: false,
                    },
                    last_scanned_block: None,
                }
            ])
        );

        // Which invocation id published the block shouldn't matter, last_scanned_block
        // should continue to move forward.
        for block_id in 456..=460 {
            let invoc_id = db
                .new_ingest_invocation(
                    None,
                    &ingress_key2,
                    &random_kex_rng_pubkey(&mut rng),
                    block_id,
                )
                .unwrap();

            let (block, records) = random_block(&mut rng, block_id, 10);
            db.add_block_data(&invoc_id, &block, 0, &records).unwrap();

            assert_eq!(
                HashSet::<IngressPublicKeyRecord>::from_iter(
                    db.get_ingress_key_records(
                        0,
                        &IngressPublicKeyRecordFilters {
                            should_include_lost_keys: true,
                            should_include_retired_keys: true,
                            should_only_include_unexpired_keys: false,
                        }
                    )
                    .unwrap()
                ),
                HashSet::from_iter(vec![
                    IngressPublicKeyRecord {
                        key: ingress_key1,
                        status: IngressPublicKeyStatus {
                            start_block: 123,
                            pubkey_expiry: 0,
                            retired: true,
                            lost: false,
                        },
                        last_scanned_block: Some(130),
                    },
                    IngressPublicKeyRecord {
                        key: ingress_key2,
                        status: IngressPublicKeyStatus {
                            start_block: 456,
                            pubkey_expiry: 888,
                            retired: false,
                            lost: false,
                        },
                        last_scanned_block: Some(block_id),
                    }
                ])
            );
        }

        // start_block_at_least filtering works as expected.
        assert_eq!(
            db.get_ingress_key_records(
                400,
                &IngressPublicKeyRecordFilters {
                    should_include_lost_keys: true,
                    should_include_retired_keys: true,
                    should_only_include_unexpired_keys: false,
                }
            )
            .unwrap(),
            vec![IngressPublicKeyRecord {
                key: ingress_key2,
                status: IngressPublicKeyStatus {
                    start_block: 456,
                    pubkey_expiry: 888,
                    retired: false,
                    lost: false,
                },
                last_scanned_block: Some(460),
            }]
        );
    }

    #[test_with_logger]
    fn test_get_ingress_key_records_should_not_include_retired_keys_does_not_return_retired_keys(
        logger: Logger,
    ) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        // At first, there are no records.
        assert_eq!(
            db.get_ingress_key_records(
                0,
                &IngressPublicKeyRecordFilters {
                    should_include_lost_keys: false,
                    should_include_retired_keys: true,
                    should_only_include_unexpired_keys: false,
                }
            )
            .unwrap(),
            vec![],
        );

        // Add an ingress key and see that we can retreive it.
        let ingress_key1 = CompressedRistrettoPublic::from_random(&mut rng);
        db.new_ingress_key(&ingress_key1, 123).unwrap();

        assert_eq!(
            db.get_ingress_key_records(
                0,
                &IngressPublicKeyRecordFilters {
                    should_include_lost_keys: true,
                    should_include_retired_keys: true,
                    should_only_include_unexpired_keys: false,
                }
            )
            .unwrap(),
            vec![IngressPublicKeyRecord {
                key: ingress_key1,
                status: IngressPublicKeyStatus {
                    start_block: 123,
                    pubkey_expiry: 0,
                    retired: false,
                    lost: false,
                },
                last_scanned_block: None,
            }],
        );
        db.retire_ingress_key(&ingress_key1, true).unwrap();
        assert_eq!(
            db.get_ingress_key_records(
                0,
                &IngressPublicKeyRecordFilters {
                    should_include_lost_keys: true,
                    should_include_retired_keys: false,
                    should_only_include_unexpired_keys: false,
                }
            )
            .unwrap()
            .len(),
            0
        );
    }

    #[test_with_logger]
    fn test_get_ingress_key_records_should_not_include_lost_keys_does_not_return_lost_keys(
        logger: Logger,
    ) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        // At first, there are no records.
        assert_eq!(
            db.get_ingress_key_records(
                0,
                &IngressPublicKeyRecordFilters {
                    should_include_lost_keys: false,
                    should_include_retired_keys: true,
                    should_only_include_unexpired_keys: false,
                }
            )
            .unwrap(),
            vec![],
        );

        // Add an ingress key and see that we can retreive it.
        let ingress_key1 = CompressedRistrettoPublic::from_random(&mut rng);
        db.new_ingress_key(&ingress_key1, 123).unwrap();

        assert_eq!(
            db.get_ingress_key_records(
                0,
                &IngressPublicKeyRecordFilters {
                    should_include_lost_keys: true,
                    should_include_retired_keys: true,
                    should_only_include_unexpired_keys: false,
                }
            )
            .unwrap(),
            vec![IngressPublicKeyRecord {
                key: ingress_key1,
                status: IngressPublicKeyStatus {
                    start_block: 123,
                    pubkey_expiry: 0,
                    retired: false,
                    lost: false,
                },
                last_scanned_block: None,
            }],
        );

        db.report_lost_ingress_key(ingress_key1).unwrap();
        assert_eq!(
            db.get_ingress_key_records(
                0,
                &IngressPublicKeyRecordFilters {
                    should_include_lost_keys: false,
                    should_include_retired_keys: true,
                    should_only_include_unexpired_keys: false,
                }
            )
            .unwrap()
            .len(),
            0
        );
    }

    #[test_with_logger]
    fn test_get_ingress_key_records_should_not_include_lost_keys_or_retired_keys_does_not_return_lost_keys_or_retired_keys(
        logger: Logger,
    ) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        // At first, there are no records.
        assert_eq!(
            db.get_ingress_key_records(
                0,
                &IngressPublicKeyRecordFilters {
                    should_include_lost_keys: false,
                    should_include_retired_keys: true,
                    should_only_include_unexpired_keys: false,
                }
            )
            .unwrap(),
            vec![],
        );

        // Add an ingress key and see that we can retreive it.
        let ingress_key1 = CompressedRistrettoPublic::from_random(&mut rng);
        db.new_ingress_key(&ingress_key1, 123).unwrap();

        let ingress_key2 = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key2, 456).unwrap();

        assert_eq!(
            HashSet::<IngressPublicKeyRecord>::from_iter(
                db.get_ingress_key_records(
                    0,
                    &IngressPublicKeyRecordFilters {
                        should_include_lost_keys: true,
                        should_include_retired_keys: true,
                        should_only_include_unexpired_keys: false,
                    }
                )
                .unwrap()
            ),
            HashSet::<IngressPublicKeyRecord>::from_iter(vec![
                IngressPublicKeyRecord {
                    key: ingress_key1,
                    status: IngressPublicKeyStatus {
                        start_block: 123,
                        pubkey_expiry: 0,
                        retired: false,
                        lost: false,
                    },
                    last_scanned_block: None,
                },
                IngressPublicKeyRecord {
                    key: ingress_key2,
                    status: IngressPublicKeyStatus {
                        start_block: 456,
                        pubkey_expiry: 0,
                        retired: false,
                        lost: false,
                    },
                    last_scanned_block: None,
                }
            ]),
        );

        db.retire_ingress_key(&ingress_key1, true).unwrap();
        db.report_lost_ingress_key(ingress_key2).unwrap();

        assert_eq!(
            db.get_ingress_key_records(
                0,
                &IngressPublicKeyRecordFilters {
                    should_include_lost_keys: false,
                    should_include_retired_keys: false,
                    should_only_include_unexpired_keys: false,
                }
            )
            .unwrap()
            .len(),
            0
        );
    }

    #[test_with_logger]
    fn test_get_ingress_key_records_should_only_include_in_use_keys_one_key_lost_one_key_retired_but_last_scanned_less_than_pubkey_expiry(
        logger: Logger,
    ) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        let proposed_start_block = 0;
        let ingress_key = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        let accepted_start_block = db
            .new_ingress_key(&ingress_key, proposed_start_block)
            .unwrap();

        assert_eq!(accepted_start_block, proposed_start_block);

        // Ensures that the accepted start block is recorded in the db.
        let ingress_key_status = db.get_ingress_key_status(&ingress_key).unwrap().unwrap();
        assert_eq!(accepted_start_block, ingress_key_status.start_block);
    }

    #[test_with_logger]
    fn test_new_ingress_key_no_blocks_added_accepted_block_count_is_proposed_start_block(
        logger: Logger,
    ) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        // At first, there are no records.
        assert_eq!(
            db.get_ingress_key_records(
                0,
                &IngressPublicKeyRecordFilters {
                    should_include_lost_keys: false,
                    should_include_retired_keys: false,
                    should_only_include_unexpired_keys: false,
                }
            )
            .unwrap(),
            vec![],
        );

        // Add an ingress key and see that we can retreive it.
        let ingress_key1 = CompressedRistrettoPublic::from_random(&mut rng);
        db.new_ingress_key(&ingress_key1, 0).unwrap();

        let ingress_key2 = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key2, 5).unwrap();

        // Add an ingest invocation and see that we can see it.
        let invoc_id1 = db
            .new_ingest_invocation(None, &ingress_key1, &random_kex_rng_pubkey(&mut rng), 123)
            .unwrap();

        let ranges = db.get_ingestable_ranges().unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start_block, 123);
        assert!(!ranges[0].decommissioned);
        assert_eq!(ranges[0].last_ingested_block, None);

        // This makes last_scanned_block equal 10.
        for block_index in 0..11 {
            let (block, records) = random_block(&mut rng, block_index, 10);
            db.add_block_data(&invoc_id1, &block, 0, &records).unwrap();
        }

        db.set_report(
            &ingress_key1,
            "",
            &ReportData {
                pubkey_expiry: 20,
                ingest_invocation_id: None,
                attestation_evidence: create_attestation_evidence(""),
            },
        )
        .unwrap();

        db.retire_ingress_key(&ingress_key1, true).unwrap();
        db.report_lost_ingress_key(ingress_key2).unwrap();

        let actual = db
            .get_ingress_key_records(
                0,
                &IngressPublicKeyRecordFilters {
                    should_include_lost_keys: false,
                    should_include_retired_keys: true,
                    should_only_include_unexpired_keys: true,
                },
            )
            .unwrap();

        let expected = IngressPublicKeyRecord {
            key: ingress_key1,
            status: IngressPublicKeyStatus {
                start_block: 0,
                pubkey_expiry: 20,
                retired: true,
                lost: false,
            },
            last_scanned_block: Some(10),
        };
        assert_eq!(actual, vec![expected]);
    }

    #[test_with_logger]
    fn test_get_ingress_key_records_should_only_include_in_use_keys_one_key_lost_one_key_retired_and_last_scanned_equal_to_pubkey_expiry(
        logger: Logger,
    ) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        let proposed_start_block = 120;
        let ingress_key = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        let accepted_start_block = db
            .new_ingress_key(&ingress_key, proposed_start_block)
            .unwrap();

        assert_eq!(accepted_start_block, proposed_start_block);

        // Ensures that the accepted start block is recorded in the db.
        let ingress_key_status = db.get_ingress_key_status(&ingress_key).unwrap().unwrap();
        assert_eq!(accepted_start_block, ingress_key_status.start_block);
    }

    #[test_with_logger]
    fn test_new_ingress_key_proposed_lower_than_highest_known_accepts_highest_known_block_count(
        logger: Logger,
    ) {
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let original_ingress_key =
            CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&original_ingress_key, 0).unwrap();
        let num_txs = 1;
        let invoc_id = db
            .new_ingest_invocation(
                None,
                &original_ingress_key,
                &random_kex_rng_pubkey(&mut rng),
                123,
            )
            .unwrap();

        // Add the 11th block to the DB. This will serve as the highest known
        // block index.
        let highest_known_block_index = 10;
        let highest_known_block_count = highest_known_block_index + 1;
        let (block, records) = random_block(&mut rng, highest_known_block_index, num_txs);
        db.add_block_data(&invoc_id, &block, 0, &records).unwrap();

        let proposed_start_block_count = 8;
        let mut rng_2: StdRng = SeedableRng::from_seed([127u8; 32]);
        let new_ingress_key =
            CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng_2));
        let accepted_start_block = db
            .new_ingress_key(&new_ingress_key, proposed_start_block_count)
            .unwrap();

        assert_eq!(accepted_start_block, highest_known_block_count);

        // Ensures that the accepted start block is recorded in the db.
        let ingress_key_status = db
            .get_ingress_key_status(&new_ingress_key)
            .unwrap()
            .unwrap();
        assert_eq!(accepted_start_block, ingress_key_status.start_block);
    }

    #[test_with_logger]
    fn test_new_ingress_key_proposed_higher_than_highest_known_accepts_proposed(logger: Logger) {
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let original_ingress_key =
            CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&original_ingress_key, 0).unwrap();
        let num_txs = 1;
        let invoc_id = db
            .new_ingest_invocation(
                None,
                &original_ingress_key,
                &random_kex_rng_pubkey(&mut rng),
                123,
            )
            .unwrap();

        // Add the 11th block to the DB. This will serve as the highest known
        // block index.
        let highest_known_block_index = 10;
        let (block, records) = random_block(&mut rng, highest_known_block_index, num_txs);
        db.add_block_data(&invoc_id, &block, 0, &records).unwrap();

        let proposed_start_block_count = 11;
        let mut rng_2: StdRng = SeedableRng::from_seed([127u8; 32]);
        let new_ingress_key =
            CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng_2));
        let accepted_start_block = db
            .new_ingress_key(&new_ingress_key, proposed_start_block_count)
            .unwrap();

        assert_eq!(accepted_start_block, proposed_start_block_count);

        // Ensures that the accepted start block is recorded in the db.
        let ingress_key_status = db
            .get_ingress_key_status(&new_ingress_key)
            .unwrap()
            .unwrap();
        assert_eq!(accepted_start_block, ingress_key_status.start_block);
    }

    #[test_with_logger]
    fn test_new_ingress_key_proposed_one_more_than_highest_known_accepts_proposed(logger: Logger) {
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let original_ingress_key =
            CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&original_ingress_key, 0).unwrap();
        let num_txs = 1;
        let invoc_id = db
            .new_ingest_invocation(
                None,
                &original_ingress_key,
                &random_kex_rng_pubkey(&mut rng),
                123,
            )
            .unwrap();

        // Add the 11th block to the DB. This will serve as the highest known
        // block index.
        let highest_known_block_index = 10;
        let highest_known_block_count = highest_known_block_index + 1;
        let (block, records) = random_block(&mut rng, highest_known_block_index, num_txs);
        db.add_block_data(&invoc_id, &block, 0, &records).unwrap();

        // Choose 10 to ensure that off by one error is accounted for.
        let proposed_start_block_count = 10;
        let mut rng_2: StdRng = SeedableRng::from_seed([127u8; 32]);
        let new_ingress_key =
            CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng_2));
        let accepted_start_block = db
            .new_ingress_key(&new_ingress_key, proposed_start_block_count)
            .unwrap();

        assert_eq!(accepted_start_block, highest_known_block_count);

        // Ensures that the accepted start block is recorded in the db.
        let ingress_key_status = db
            .get_ingress_key_status(&new_ingress_key)
            .unwrap()
            .unwrap();
        assert_eq!(accepted_start_block, ingress_key_status.start_block);
    }

    #[test_with_logger]
    fn test_get_ingress_key_records_should_only_include_in_use_keys_one_key_lost_one_key_retired_and_last_scanned_greater_than_pubkey_expiry(
        logger: Logger,
    ) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        // At first, there are no records.
        assert_eq!(
            db.get_ingress_key_records(
                0,
                &IngressPublicKeyRecordFilters {
                    should_include_lost_keys: false,
                    should_include_retired_keys: false,
                    should_only_include_unexpired_keys: false,
                }
            )
            .unwrap(),
            vec![],
        );

        // Add an ingress key and see that we can retreive it.
        let ingress_key1 = CompressedRistrettoPublic::from_random(&mut rng);
        db.new_ingress_key(&ingress_key1, 0).unwrap();

        let ingress_key2 = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key2, 5).unwrap();

        // Add an ingest invocation and see that we can see it.
        let invoc_id1 = db
            .new_ingest_invocation(None, &ingress_key1, &random_kex_rng_pubkey(&mut rng), 123)
            .unwrap();

        let ranges = db.get_ingestable_ranges().unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].id, invoc_id1);
        assert_eq!(ranges[0].start_block, 123);
        assert!(!ranges[0].decommissioned);
        assert_eq!(ranges[0].last_ingested_block, None);

        // This makes last_scanned_block equal 10.
        for block_index in 0..11 {
            let (block, records) = random_block(&mut rng, block_index, 10);
            db.add_block_data(&invoc_id1, &block, 0, &records).unwrap();
        }

        db.set_report(
            &ingress_key1,
            "",
            &ReportData {
                pubkey_expiry: 5,
                ingest_invocation_id: None,
                attestation_evidence: create_attestation_evidence(""),
            },
        )
        .unwrap();

        db.retire_ingress_key(&ingress_key1, true).unwrap();
        db.report_lost_ingress_key(ingress_key2).unwrap();

        let actual = db
            .get_ingress_key_records(
                0,
                &IngressPublicKeyRecordFilters {
                    should_include_lost_keys: false,
                    should_include_retired_keys: true,
                    should_only_include_unexpired_keys: true,
                },
            )
            .unwrap();

        assert_eq!(actual.len(), 0);
    }

    #[test_with_logger]
    fn test_get_ingress_key_records_should_only_include_in_use_keys_one_key_lost_one_key_retired_and_last_scanned_less_than_pubkey_expiry(
        logger: Logger,
    ) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        // At first, there are no records.
        assert_eq!(
            db.get_ingress_key_records(
                0,
                &IngressPublicKeyRecordFilters {
                    should_include_lost_keys: false,
                    should_include_retired_keys: false,
                    should_only_include_unexpired_keys: false,
                }
            )
            .unwrap(),
            vec![],
        );

        // Add an ingress key and see that we can retreive it.
        let ingress_key1 = CompressedRistrettoPublic::from_random(&mut rng);
        db.new_ingress_key(&ingress_key1, 0).unwrap();

        let ingress_key2 = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key2, 5).unwrap();

        // Add an ingest invocation and see that we can see it.
        let invoc_id1 = db
            .new_ingest_invocation(None, &ingress_key1, &random_kex_rng_pubkey(&mut rng), 123)
            .unwrap();

        let ranges = db.get_ingestable_ranges().unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].id, invoc_id1);
        assert_eq!(ranges[0].start_block, 123);
        assert!(!ranges[0].decommissioned);
        assert_eq!(ranges[0].last_ingested_block, None);

        // This makes last_scanned_block equal 10.
        for block_index in 0..11 {
            let (block, records) = random_block(&mut rng, block_index, 10);
            db.add_block_data(&invoc_id1, &block, 0, &records).unwrap();
        }

        db.set_report(
            &ingress_key1,
            "",
            &ReportData {
                pubkey_expiry: 15,
                ingest_invocation_id: None,
                attestation_evidence: create_attestation_evidence(""),
            },
        )
        .unwrap();

        db.retire_ingress_key(&ingress_key1, true).unwrap();
        db.report_lost_ingress_key(ingress_key2).unwrap();

        let actual = db
            .get_ingress_key_records(
                0,
                &IngressPublicKeyRecordFilters {
                    should_include_lost_keys: false,
                    should_include_retired_keys: true,
                    should_only_include_unexpired_keys: true,
                },
            )
            .unwrap();

        let expected = IngressPublicKeyRecord {
            key: ingress_key1,
            status: IngressPublicKeyStatus {
                start_block: 0,
                pubkey_expiry: 15,
                retired: true,
                lost: false,
            },
            last_scanned_block: Some(10),
        };
        assert_eq!(actual, vec![expected]);
    }

    #[test_with_logger]
    fn get_expired_invocations_multiple_expired(logger: Logger) {
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        let mut rng = thread_rng();
        let ingress_key = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key, 0).unwrap();

        let mut egress_keys = Vec::new();
        for _ in 0..3 {
            let egress_key = random_kex_rng_pubkey(&mut rng);
            let _invoc_id = db
                .new_ingest_invocation(None, &ingress_key, &egress_key, 0)
                .unwrap();
            egress_keys.push(egress_key);
        }

        // This buffer allows us to be sure that the db entries will be before the
        // expiration.
        let expiration_buffer = Duration::from_secs(1).as_secs() as i64;
        let expiration_timestamp: i64 = Utc::now().timestamp() + expiration_buffer;
        let expiration = NaiveDateTime::from_timestamp_opt(expiration_timestamp, 0).unwrap();

        let result = db.get_expired_invocations(expiration);

        assert!(result.is_ok());
        let result = result.unwrap();

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].egress_public_key, egress_keys[0]);
        assert_eq!(result[1].egress_public_key, egress_keys[1]);
        assert_eq!(result[2].egress_public_key, egress_keys[2]);
    }

    #[test_with_logger]
    fn get_expired_invocations_mixed(logger: Logger) {
        let db_test_context = test_utils::SqlRecoveryDbTestContext::new(logger);
        let db = db_test_context.get_db_instance();

        let mut rng = thread_rng();
        let ingress_key = CompressedRistrettoPublic::from(RistrettoPublic::from_random(&mut rng));
        db.new_ingress_key(&ingress_key, 0).unwrap();

        let egress_key_1 = random_kex_rng_pubkey(&mut rng);
        let _invoc_id = db
            .new_ingest_invocation(None, &ingress_key, &egress_key_1, 0)
            .unwrap();

        // This buffer allows us to be sure that the db entries will be before the
        // expiration.
        let expiration_buffer = Duration::from_secs(1).as_secs() as i64;
        let expiration_timestamp: i64 = Utc::now().timestamp() + expiration_buffer;
        let expiration = NaiveDateTime::from_timestamp_opt(expiration_timestamp, 0).unwrap();

        // Sleep to ensure that this second ingest invocation is not expired.
        std::thread::sleep(Duration::from_secs(2));
        let egress_key_2 = random_kex_rng_pubkey(&mut rng);
        let _invoc_id = db
            .new_ingest_invocation(None, &ingress_key, &egress_key_2, 0)
            .unwrap();

        let result = db.get_expired_invocations(expiration);

        assert!(result.is_ok());
        let result = result.unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].egress_public_key, egress_key_1);
    }
}
