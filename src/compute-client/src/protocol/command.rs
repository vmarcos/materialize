// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Compute protocol commands.

use mz_cluster_client::client::{ClusterStartupEpoch, TimelyConfig, TryIntoTimelyConfig};
use mz_compute_types::dataflows::{DataflowDescription, YieldSpec};
use mz_expr::RowSetFinishing;
use mz_ore::tracing::OpenTelemetryContext;
use mz_persist_client::cfg::PersistParameters;
use mz_proto::{any_uuid, IntoRustIfSome, ProtoType, RustType, TryFromProtoError};
use mz_repr::{GlobalId, Row};
use mz_service::params::GrpcClientParameters;
use mz_storage_client::client::ProtoCompaction;
use mz_storage_types::controller::CollectionMetadata;
use mz_timely_util::progress::any_antichain;
use mz_tracing::params::TracingParameters;
use proptest::prelude::{any, Arbitrary};
use proptest::strategy::{BoxedStrategy, Strategy, Union};
use proptest_derive::Arbitrary;
use serde::{Deserialize, Serialize};
use timely::progress::frontier::Antichain;
use uuid::Uuid;

use crate::logging::LoggingConfig;

include!(concat!(
    env!("OUT_DIR"),
    "/mz_compute_client.protocol.command.rs"
));

/// Compute protocol commands, sent by the compute controller to replicas.
///
/// Command sequences sent by the compute controller must be valid according to the [Protocol
/// Stages].
///
/// [Protocol Stages]: super#protocol-stages
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ComputeCommand<T = mz_repr::Timestamp> {
    /// `CreateTimely` is the first command sent to a replica after a connection was established.
    /// It instructs the replica to initialize the timely dataflow runtime using the given
    /// `config`.
    ///
    /// This command is special in that it is the only one that is broadcast to all processes of a
    /// multi-process replica. All subsequent commands are only sent to the first process, which
    /// then distributes them to the other processes using a dataflow. This method of command
    /// distribution requires the timely dataflow runtime to be initialized, which is why the
    /// `CreateTimely` command exists.
    ///
    /// The `epoch` value imposes an ordering on iterations of the compute protocol. When the
    /// compute controller connects to a replica, it must send an `epoch` that is greater than all
    /// epochs it sent to the same replica on previous connections. Multi-process replicas should
    /// use the `epoch` to ensure that their individual processes agree on which protocol iteration
    /// they are in.
    CreateTimely {
        config: TimelyConfig,
        epoch: ClusterStartupEpoch,
    },

    /// `CreateInstance` must be sent after `CreateTimely` to complete the [Creation Stage] of the
    /// compute protocol. Unlike `CreateTimely`, and like all other commands, it is only sent to
    /// the first process of the replica, and then distributed through the timely runtime.
    /// `CreateInstance` instructs the replica to initialize its state to a point where it is ready
    /// to start maintaining dataflows.
    ///
    /// Upon receiving a `CreateInstance` command, the replica must further initialize logging
    /// dataflows according to the given [`LoggingConfig`].
    ///
    /// [Creation Stage]: super#creation-stage
    CreateInstance(InstanceConfig),

    /// `InitializationComplete` informs the replica about the end of the [Initialization Stage].
    /// Upon receiving this command, the replica should perform a reconciliation process, to ensure
    /// its dataflow state matches the state requested by the computation commands it received
    /// previously. The replica must now start sending responses to commands received previously,
    /// if it opted to defer them during the [Initialization Stage].
    ///
    /// [Initialization Stage]: super#initialization-stage
    InitializationComplete,

    /// `UpdateConfiguration` instructs the replica to update its configuration, according to the
    /// given [`ComputeParameters`].
    ///
    /// Parameter updates transmitted through this command must be applied by the replica as soon
    /// as it receives the command, and they must be applied globally to all replica state, even
    /// dataflows and pending peeks that were created before the parameter update. This property
    /// allows the replica to hoist `UpdateConfiguration` commands during reconciliation.
    ///
    /// Configuration parameters that should not be applied globally, but only to specific
    /// dataflows or peeks, should be added to the [`DataflowDescription`] or [`Peek`] types,
    /// rather than as [`ComputeParameters`].
    UpdateConfiguration(ComputeParameters),

    /// `CreateDataflow` instructs the replica to create and start maintaining a dataflow according
    /// to the given [`DataflowDescription`].
    ///
    /// The [`DataflowDescription`] must have the following properties:
    ///
    ///   * Dataflow imports are valid:
    ///     * Imported storage collections specified in [`source_imports`] exist and are readable by
    ///       the compute replica.
    ///     * Imported indexes specified in [`index_imports`] have been created on the replica
    ///       previously, by previous `CreateDataflow` commands.
    ///   * Dataflow imports are readable at the specified [`as_of`]. In other words: The `since`s of
    ///     imported collections are not beyond the dataflow [`as_of`].
    ///   * Dataflow exports have unique IDs, i.e., the IDs of exports from dataflows a replica is
    ///     instructed to create do not repeat (within a single protocol iteration).
    ///   * The dataflow objects defined in [`objects_to_build`] are topologically ordered according
    ///     to the dependency relation.
    ///
    /// A dataflow description that violates any of the above properties can cause the replica to
    /// exhibit undefined behavior, such as panicking or production of incorrect results. A replica
    /// should prefer panicking over producing incorrect results.
    ///
    /// After receiving a `CreateDataflow` command, if the created dataflow exports indexes or
    /// storage sinks, the replica must produce [`FrontierUpper`] responses that report the
    /// advancement of the `upper` frontiers of these compute collections.
    ///
    /// After receiving a `CreateDataflow` command, if the created dataflow exports subscribes, the
    /// replica must produce [`SubscribeResponse`]s that report the progress and results of the
    /// subscribes.
    ///
    /// [`objects_to_build`]: DataflowDescription::objects_to_build
    /// [`source_imports`]: DataflowDescription::source_imports
    /// [`index_imports`]: DataflowDescription::index_imports
    /// [`as_of`]: DataflowDescription::as_of
    /// [`FrontierUpper`]: super::response::ComputeResponse::FrontierUpper
    /// [`SubscribeResponse`]: super::response::ComputeResponse::SubscribeResponse
    /// [Initialization Stage]: super#initialization-stage
    CreateDataflow(DataflowDescription<mz_compute_types::plan::Plan<T>, CollectionMetadata, T>),

    /// `AllowCompaction` informs the replica about the relaxation of external read capabilities on
    /// a compute collection exported by one of the replica’s dataflow.
    ///
    /// The command names a collection and provides a frontier after which accumulations must be
    /// correct. The replica gains the liberty of compacting the corresponding maintained trace up
    /// through that frontier.
    ///
    /// It is invalid to send an `AllowCompaction` command that references a compute collection
    /// that was not created by a corresponding `CreateDataflow` command before. Doing so may cause
    /// the replica to exhibit undefined behavior.
    ///
    /// The `AllowCompaction` command only informs about external read requirements, not internal
    /// ones. The replica is responsible for ensuring that internal requirements are fulfilled at
    /// all times, so local dataflow inputs are not compacted beyond times at which they are still
    /// being read from.
    ///
    /// The read frontiers transmitted through `AllowCompaction`s may be beyond the corresponding
    /// collections' current `upper` frontiers. This signals that external readers are not
    /// interested in times up to the specified new read frontiers. Consequently, an empty read
    /// frontier signals that external readers are not interested in updates from the corresponding
    /// collection ever again, so the collection is not required anymore.
    ///
    /// Sending an `AllowCompaction` command with the empty frontier is the canonical way to drop
    /// compute collections.
    ///
    /// A replica that receives an `AllowCompaction` command with the empty frontier must
    /// eventually respond with a [`FrontierUpper`] response reporting the empty frontier for the
    /// same collection. ([#16275])
    ///
    /// [`FrontierUpper`]: super::response::ComputeResponse::FrontierUpper
    /// [#16275]: https://github.com/MaterializeInc/materialize/issues/16275
    AllowCompaction {
        id: GlobalId,
        frontier: Antichain<T>,
    },

    /// `Peek` instructs the replica to perform a peek on a collection: either an index or a
    /// Persist-backed collection.
    ///
    /// The [`Peek`] description must have the following properties:
    ///
    ///   * If targeting an index, it has previously been created by a corresponding `CreateDataflow`
    ///     command. (If targeting a persist collection, that collection should exist.)
    ///   * The [`Peek::uuid`] is unique, i.e., the UUIDs of peeks a replica gets instructed to
    ///     perform do not repeat (within a single protocol iteration).
    ///
    /// A [`Peek`] description that violates any of the above properties can cause the replica to
    /// exhibit undefined behavior.
    ///
    /// Specifying a [`Peek::timestamp`] that is less than the target index’s `since` frontier does
    /// not provoke undefined behavior. Instead, the replica must produce a [`PeekResponse::Error`]
    /// in response.
    ///
    /// After receiving a `Peek` command, the replica must eventually produce a single
    /// [`PeekResponse`]:
    ///
    ///    * For peeks that were not cancelled: either [`Rows`] or [`Error`].
    ///    * For peeks that were cancelled: either [`Rows`], or [`Error`], or [`Canceled`].
    ///
    /// [`PeekResponse`]: super::response::PeekResponse
    /// [`PeekResponse::Error`]: super::response::PeekResponse::Error
    /// [`Rows`]: super::response::PeekResponse::Rows
    /// [`Error`]: super::response::PeekResponse::Error
    /// [`Canceled`]: super::response::PeekResponse::Canceled
    Peek(Peek<T>),

    /// `CancelPeek` instructs the replica to cancel the identified pending peek.
    ///
    /// It is invalid to send a `CancelPeek` command that references a peek that was not created
    /// by a corresponding `Peek` command before. Doing so may cause the replica to exhibit
    /// undefined behavior.
    ///
    /// If a replica cancels a peek in response to a `CancelPeek` command, it must respond with a
    /// [`PeekResponse::Canceled`]. The replica may also decide to fulfill the peek instead and
    /// return a different [`PeekResponse`], or it may already have returned a response to the
    /// specified peek. In these cases it must *not* return another [`PeekResponse`].
    ///
    /// [`PeekResponse`]: super::response::PeekResponse
    /// [`PeekResponse::Canceled`]: super::response::PeekResponse::Canceled
    CancelPeek {
        /// The identifier of the peek request to cancel.
        ///
        /// This Value must match a [`Peek::uuid`] value transmitted in a previous `Peek` command.
        uuid: Uuid,
    },
}

impl RustType<ProtoComputeCommand> for ComputeCommand<mz_repr::Timestamp> {
    fn into_proto(&self) -> ProtoComputeCommand {
        use proto_compute_command::Kind::*;
        use proto_compute_command::*;
        ProtoComputeCommand {
            kind: Some(match self {
                ComputeCommand::CreateTimely { config, epoch } => CreateTimely(ProtoCreateTimely {
                    config: Some(config.into_proto()),
                    epoch: Some(epoch.into_proto()),
                }),
                ComputeCommand::CreateInstance(config) => CreateInstance(config.into_proto()),
                ComputeCommand::InitializationComplete => InitializationComplete(()),
                ComputeCommand::UpdateConfiguration(params) => {
                    UpdateConfiguration(params.into_proto())
                }
                ComputeCommand::CreateDataflow(dataflow) => CreateDataflow(dataflow.into_proto()),
                ComputeCommand::AllowCompaction { id, frontier } => {
                    AllowCompaction(ProtoCompaction {
                        id: Some(id.into_proto()),
                        frontier: Some(frontier.into_proto()),
                    })
                }
                ComputeCommand::Peek(peek) => Peek(peek.into_proto()),
                ComputeCommand::CancelPeek { uuid } => CancelPeek(uuid.into_proto()),
            }),
        }
    }

    fn from_proto(proto: ProtoComputeCommand) -> Result<Self, TryFromProtoError> {
        use proto_compute_command::Kind::*;
        use proto_compute_command::*;
        match proto.kind {
            Some(CreateTimely(ProtoCreateTimely { config, epoch })) => {
                Ok(ComputeCommand::CreateTimely {
                    config: config.into_rust_if_some("ProtoCreateTimely::config")?,
                    epoch: epoch.into_rust_if_some("ProtoCreateTimely::epoch")?,
                })
            }
            Some(CreateInstance(config)) => Ok(ComputeCommand::CreateInstance(config.into_rust()?)),
            Some(InitializationComplete(())) => Ok(ComputeCommand::InitializationComplete),
            Some(UpdateConfiguration(params)) => {
                Ok(ComputeCommand::UpdateConfiguration(params.into_rust()?))
            }
            Some(CreateDataflow(dataflow)) => {
                Ok(ComputeCommand::CreateDataflow(dataflow.into_rust()?))
            }
            Some(AllowCompaction(ProtoCompaction { id, frontier })) => {
                Ok(ComputeCommand::AllowCompaction {
                    id: id.into_rust_if_some("ProtoAllowCompaction::id")?,
                    frontier: frontier.into_rust_if_some("ProtoAllowCompaction::frontier")?,
                })
            }
            Some(Peek(peek)) => Ok(ComputeCommand::Peek(peek.into_rust()?)),
            Some(CancelPeek(uuid)) => Ok(ComputeCommand::CancelPeek {
                uuid: uuid.into_rust()?,
            }),
            None => Err(TryFromProtoError::missing_field(
                "ProtoComputeCommand::kind",
            )),
        }
    }
}

impl Arbitrary for ComputeCommand<mz_repr::Timestamp> {
    type Strategy = Union<BoxedStrategy<Self>>;
    type Parameters = ();

    fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
        Union::new(vec![
            any::<InstanceConfig>()
                .prop_map(ComputeCommand::CreateInstance)
                .boxed(),
            any::<ComputeParameters>()
                .prop_map(ComputeCommand::UpdateConfiguration)
                .boxed(),
            any::<
                DataflowDescription<
                    mz_compute_types::plan::Plan,
                    CollectionMetadata,
                    mz_repr::Timestamp,
                >,
            >()
            .prop_map(ComputeCommand::CreateDataflow)
            .boxed(),
            (any::<GlobalId>(), any_antichain())
                .prop_map(|(id, frontier)| ComputeCommand::AllowCompaction { id, frontier })
                .boxed(),
            any::<Peek>().prop_map(ComputeCommand::Peek).boxed(),
            any_uuid()
                .prop_map(|uuid| ComputeCommand::CancelPeek { uuid })
                .boxed(),
        ])
    }
}

/// Configuration for a replica, passed with the `CreateInstance`. Replicas should halt
/// if the controller attempt to reconcile them with different values
/// for anything in this struct.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Arbitrary)]
pub struct InstanceConfig {
    pub logging: LoggingConfig,
}

impl RustType<ProtoInstanceConfig> for InstanceConfig {
    fn into_proto(&self) -> ProtoInstanceConfig {
        ProtoInstanceConfig {
            logging: Some(self.logging.into_proto()),
        }
    }

    fn from_proto(proto: ProtoInstanceConfig) -> Result<Self, TryFromProtoError> {
        Ok(Self {
            logging: proto
                .logging
                .into_rust_if_some("ProtoCreateInstance::logging")?,
        })
    }
}

/// Compute instance configuration parameters.
///
/// Parameters can be set (`Some`) or unset (`None`).
/// Unset parameters should be interpreted to mean "use the previous value".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Arbitrary)]
pub struct ComputeParameters {
    /// The maximum allowed size in bytes for results of peeks and subscribes.
    ///
    /// Peeks and subscribes that would return results larger than this maximum return the
    /// respective error responses instead:
    ///   * [`PeekResponse::Rows`] is replaced by [`PeekResponse::Error`].
    ///   * The [`SubscribeBatch::updates`] field is populated with an [`Err`] value.
    ///
    /// [`PeekResponse::Rows`]: super::response::PeekResponse::Rows
    /// [`PeekResponse::Error`]: super::response::PeekResponse::Error
    /// [`SubscribeBatch::updates`]: super::response::SubscribeBatch::updates
    pub max_result_size: Option<u64>,
    /// The maximum number of in-flight bytes emitted by persist_sources feeding
    /// dataflows.
    ///
    /// NB: This value is optional, so the outer option indicates if this update
    /// includes an override and the inner option is part of the config value.
    pub dataflow_max_inflight_bytes: Option<Option<usize>>,
    /// The yielding behavior with which linear joins should be rendered.
    pub linear_join_yielding: Option<YieldSpec>,
    /// Whether rendering should use `mz_join_core` rather than DD's `JoinCore::join_core`.
    pub enable_mz_join_core: Option<bool>,
    /// Whether to activate jemalloc heap profiling.
    pub enable_jemalloc_profiling: Option<bool>,
    /// Enable arrangement type specialization.
    pub enable_specialized_arrangements: Option<bool>,
    /// Enable lgalloc for columnation.
    pub enable_columnation_lgalloc: Option<bool>,
    /// Persist client configuration.
    pub persist: PersistParameters,
    /// Tracing configuration.
    pub tracing: TracingParameters,
    /// gRPC client configuration.
    pub grpc_client: GrpcClientParameters,
}

impl ComputeParameters {
    /// Update the parameter values with the set ones from `other`.
    pub fn update(&mut self, other: ComputeParameters) {
        let ComputeParameters {
            max_result_size,
            dataflow_max_inflight_bytes,
            linear_join_yielding,
            enable_mz_join_core,
            enable_jemalloc_profiling,
            enable_specialized_arrangements,
            enable_columnation_lgalloc,
            persist,
            tracing,
            grpc_client,
        } = other;

        if max_result_size.is_some() {
            self.max_result_size = max_result_size;
        }
        if dataflow_max_inflight_bytes.is_some() {
            self.dataflow_max_inflight_bytes = dataflow_max_inflight_bytes;
        }
        if linear_join_yielding.is_some() {
            self.linear_join_yielding = linear_join_yielding;
        }
        if enable_mz_join_core.is_some() {
            self.enable_mz_join_core = enable_mz_join_core;
        }
        if enable_jemalloc_profiling.is_some() {
            self.enable_jemalloc_profiling = enable_jemalloc_profiling;
        }

        if enable_specialized_arrangements.is_some() {
            self.enable_specialized_arrangements = enable_specialized_arrangements;
        }

        if enable_columnation_lgalloc.is_some() {
            self.enable_columnation_lgalloc = enable_columnation_lgalloc;
        }

        self.persist.update(persist);
        self.tracing.update(tracing);
        self.grpc_client.update(grpc_client);
    }

    /// Return whether all parameters are unset.
    pub fn all_unset(&self) -> bool {
        self.max_result_size.is_none() && self.persist.all_unset() && self.grpc_client.all_unset()
    }
}

impl RustType<ProtoComputeParameters> for ComputeParameters {
    fn into_proto(&self) -> ProtoComputeParameters {
        ProtoComputeParameters {
            max_result_size: self.max_result_size.into_proto(),
            dataflow_max_inflight_bytes: self.dataflow_max_inflight_bytes.map(|x| {
                ProtoComputeMaxInflightBytesConfig {
                    dataflow_max_inflight_bytes: x.into_proto(),
                }
            }),
            linear_join_yielding: self.linear_join_yielding.into_proto(),
            enable_mz_join_core: self.enable_mz_join_core.into_proto(),
            enable_jemalloc_profiling: self.enable_jemalloc_profiling.into_proto(),
            enable_specialized_arrangements: self.enable_specialized_arrangements.into_proto(),
            enable_columnation_lgalloc: self.enable_columnation_lgalloc.into_proto(),
            persist: Some(self.persist.into_proto()),
            tracing: Some(self.tracing.into_proto()),
            grpc_client: Some(self.grpc_client.into_proto()),
        }
    }

    fn from_proto(proto: ProtoComputeParameters) -> Result<Self, TryFromProtoError> {
        Ok(Self {
            max_result_size: proto.max_result_size.into_rust()?,
            dataflow_max_inflight_bytes: proto
                .dataflow_max_inflight_bytes
                .map(|x| x.dataflow_max_inflight_bytes.into_rust())
                .transpose()?,
            linear_join_yielding: proto.linear_join_yielding.into_rust()?,
            enable_mz_join_core: proto.enable_mz_join_core.into_rust()?,
            enable_jemalloc_profiling: proto.enable_jemalloc_profiling.into_rust()?,
            enable_specialized_arrangements: proto.enable_specialized_arrangements.into_rust()?,
            enable_columnation_lgalloc: proto.enable_columnation_lgalloc.into_rust()?,
            persist: proto
                .persist
                .into_rust_if_some("ProtoComputeParameters::persist")?,
            tracing: proto
                .tracing
                .into_rust_if_some("ProtoComputeParameters::tracing")?,
            grpc_client: proto
                .grpc_client
                .into_rust_if_some("ProtoComputeParameters::grpc_client")?,
        })
    }
}

/// Metadata specific to the peek variant.
#[derive(Arbitrary, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PeekTarget {
    /// This peek is against an index. Since this should be held in memory on
    /// the target cluster, no additional coordinates are necessary.
    Index {
        /// The id of the (possibly transient) index.
        id: GlobalId,
    },
    /// This peek is against a Persist collection.
    Persist {
        /// The id of the backing Persist collection.
        id: GlobalId,
        /// The identifying metadata of the Persist shard.
        metadata: CollectionMetadata,
    },
}

impl PeekTarget {
    pub fn id(&self) -> GlobalId {
        match self {
            PeekTarget::Index { id, .. } => *id,
            PeekTarget::Persist { id, .. } => *id,
        }
    }
}

/// Peek a collection, either in an arrangement or Persist.
///
/// This request elicits data from the worker, by naming the
/// collection and some actions to apply to the results before
/// returning them.
///
/// The `timestamp` member must be valid for the arrangement that
/// is referenced by `id`. This means that `AllowCompaction` for
/// this arrangement should not pass `timestamp` before this command.
/// Subsequent commands may arbitrarily compact the arrangements;
/// the dataflow runners are responsible for ensuring that they can
/// correctly answer the `Peek`.
#[derive(Arbitrary, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Peek<T = mz_repr::Timestamp> {
    /// Target-specific metadata.
    pub target: PeekTarget,
    /// If `Some`, then look up only the given keys from the collection (instead of a full scan).
    /// The vector is never empty.
    #[proptest(strategy = "proptest::option::of(proptest::collection::vec(any::<Row>(), 1..5))")]
    pub literal_constraints: Option<Vec<Row>>,
    /// The identifier of this peek request.
    ///
    /// Used in responses and cancellation requests.
    #[proptest(strategy = "any_uuid()")]
    pub uuid: Uuid,
    /// The logical timestamp at which the collection is queried.
    pub timestamp: T,
    /// Actions to apply to the result set before returning them.
    pub finishing: RowSetFinishing,
    /// Linear operation to apply in-line on each result.
    pub map_filter_project: mz_expr::SafeMfpPlan,
    /// An `OpenTelemetryContext` to forward trace information along
    /// to the compute worker to allow associating traces between
    /// the compute controller and the compute worker.
    #[proptest(strategy = "empty_otel_ctx()")]
    pub otel_ctx: OpenTelemetryContext,
}

impl RustType<ProtoPeek> for Peek {
    fn into_proto(&self) -> ProtoPeek {
        ProtoPeek {
            id: Some(self.target.id().into_proto()),
            key: match &self.literal_constraints {
                // In the Some case, the vector is never empty, so it's safe to encode None as an
                // empty vector, and Some(vector) as just the vector.
                Some(vec) => {
                    assert!(!vec.is_empty());
                    vec.into_proto()
                }
                None => Vec::<Row>::new().into_proto(),
            },
            uuid: Some(self.uuid.into_proto()),
            timestamp: self.timestamp.into(),
            finishing: Some(self.finishing.into_proto()),
            map_filter_project: Some(self.map_filter_project.into_proto()),
            otel_ctx: self.otel_ctx.clone().into(),
            target: Some(match &self.target {
                PeekTarget::Index { id } => proto_peek::Target::Index(ProtoIndexTarget {
                    id: Some(id.into_proto()),
                }),

                PeekTarget::Persist { id, metadata } => {
                    proto_peek::Target::Persist(ProtoPersistTarget {
                        id: Some(id.into_proto()),
                        metadata: Some(metadata.into_proto()),
                    })
                }
            }),
        }
    }

    fn from_proto(x: ProtoPeek) -> Result<Self, TryFromProtoError> {
        Ok(Self {
            literal_constraints: {
                let vec: Vec<Row> = x.key.into_rust()?;
                if vec.is_empty() {
                    None
                } else {
                    Some(vec)
                }
            },
            uuid: x.uuid.into_rust_if_some("ProtoPeek::uuid")?,
            timestamp: x.timestamp.into(),
            finishing: x.finishing.into_rust_if_some("ProtoPeek::finishing")?,
            map_filter_project: x
                .map_filter_project
                .into_rust_if_some("ProtoPeek::map_filter_project")?,
            otel_ctx: x.otel_ctx.into(),
            target: match x.target {
                Some(proto_peek::Target::Index(target)) => PeekTarget::Index {
                    id: target.id.into_rust_if_some("ProtoIndexTarget::id")?,
                },
                Some(proto_peek::Target::Persist(target)) => PeekTarget::Persist {
                    id: target.id.into_rust_if_some("ProtoIndexTarget::id")?,
                    metadata: target.metadata.into_rust_if_some("ProtoPeek::target")?,
                },
                None => PeekTarget::Index {
                    id: x.id.into_rust_if_some("ProtoPeek::id")?,
                },
            },
        })
    }
}

fn empty_otel_ctx() -> impl Strategy<Value = OpenTelemetryContext> {
    (0..1).prop_map(|_| OpenTelemetryContext::empty())
}

impl TryIntoTimelyConfig for ComputeCommand {
    fn try_into_timely_config(self) -> Result<(TimelyConfig, ClusterStartupEpoch), Self> {
        match self {
            ComputeCommand::CreateTimely { config, epoch } => Ok((config, epoch)),
            cmd => Err(cmd),
        }
    }
}

#[cfg(test)]
mod tests {
    use mz_proto::protobuf_roundtrip;
    use proptest::prelude::ProptestConfig;
    use proptest::proptest;

    use super::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[mz_ore::test]
        #[cfg_attr(miri, ignore)] // error: unsupported operation: can't call foreign function `decContextDefault` on OS `linux`
        fn peek_protobuf_roundtrip(expect in any::<Peek>() ) {
            let actual = protobuf_roundtrip::<_, ProtoPeek>(&expect);
            assert!(actual.is_ok());
            assert_eq!(actual.unwrap(), expect);
        }

        #[mz_ore::test]
        fn compute_command_protobuf_roundtrip(expect in any::<ComputeCommand<mz_repr::Timestamp>>() ) {
            let actual = protobuf_roundtrip::<_, ProtoComputeCommand>(&expect);
            assert!(actual.is_ok());
            assert_eq!(actual.unwrap(), expect);
        }
    }
}
