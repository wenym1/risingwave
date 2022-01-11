use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::anyhow;
use log::info;
use risingwave_common::error::Result;
use warp::Filter;

use crate::cluster::{StoredClusterManager, WorkerNodeMetaManager};
use crate::stream::StoredStreamMetaManager;

#[derive(Clone)]
pub struct DashboardService {
    pub dashboard_addr: SocketAddr,
    pub cluster_manager: Arc<StoredClusterManager>,
    pub stream_meta_manager: Arc<StoredStreamMetaManager>,
    pub has_test_data: Arc<AtomicBool>,
}

pub type Service = Arc<DashboardService>;
use std::convert::Infallible;

mod handlers {
    use itertools::Itertools;
    use risingwave_common::array::RwError;
    use risingwave_common::error::ToRwResult;
    use risingwave_pb::common::WorkerNode;
    use risingwave_pb::meta::ActorLocation;
    use risingwave_pb::stream_plan::{stream_node, Dispatcher, StreamActor, StreamNode};
    use serde::{Serialize, Serializer};
    use warp::reject::Reject;

    use super::*;
    use crate::stream::StreamMetaManager;

    #[derive(Debug)]
    pub struct RwMetaError {
        error: RwError,
    }

    impl Serialize for RwMetaError {
        fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            serializer.serialize_str(&format!("{:?}", self))
        }
    }

    impl From<RwError> for RwMetaError {
        fn from(error: RwError) -> Self {
            Self { error }
        }
    }

    impl Reject for RwMetaError {}

    pub type MetaResult<T> = std::result::Result<T, RwMetaError>;

    #[derive(Serialize)]
    pub struct JsonWorkerNode {
        pub id: u32,
        pub host: String,
        pub port: i32,
    }

    impl From<&WorkerNode> for JsonWorkerNode {
        fn from(that: &WorkerNode) -> Self {
            Self {
                id: that.get_id(),
                host: that.get_host().get_host().to_owned(),
                port: that.get_host().get_port(),
            }
        }
    }

    pub async fn list_clusters_inner(ty: i32, srv: Service) -> MetaResult<Vec<JsonWorkerNode>> {
        srv.add_test_data().await?;

        use risingwave_pb::meta::ClusterType;
        let result = srv
            .cluster_manager
            .list_worker_node(
                ClusterType::from_i32(ty)
                    .ok_or_else(|| anyhow!("invalid cluster type"))
                    .to_rw_result()?,
            ) // TODO: error handling
            .await?
            .iter()
            .map(JsonWorkerNode::from)
            .collect_vec(); // TODO: handle error
        Ok(result)
    }

    pub async fn list_clusters(ty: i32, srv: Service) -> impl warp::Reply {
        match list_clusters_inner(ty, srv).await {
            Ok(reply) => {
                warp::reply::with_status(warp::reply::json(&reply), warp::http::StatusCode::OK)
            }
            Err(err) => warp::reply::with_status(
                warp::reply::json(&err),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            ),
        }
    }

    #[derive(Serialize)]
    pub struct JsonActorLocation {
        node: JsonWorkerNode,
        actor: Vec<JsonStreamActor>,
    }

    impl From<&ActorLocation> for JsonActorLocation {
        fn from(that: &ActorLocation) -> Self {
            JsonActorLocation {
                node: that.get_node().into(),
                actor: that
                    .get_actors()
                    .iter()
                    .map(JsonStreamActor::from)
                    .collect(),
            }
        }
    }

    #[derive(Serialize)]
    pub struct JsonStreamActor {
        actor_id: u32,
        nodes: JsonStreamNode,
        dispatcher: JsonDispatcher,
        downstream_actor_id: Vec<u32>,
    }

    impl From<&StreamActor> for JsonStreamActor {
        fn from(that: &StreamActor) -> Self {
            JsonStreamActor {
                actor_id: that.get_actor_id(),
                nodes: that.get_nodes().into(),
                dispatcher: that.get_dispatcher().into(),
                downstream_actor_id: that.get_downstream_actor_id().clone(),
            }
        }
    }

    #[derive(Serialize)]
    pub struct JsonDispatcher {
        dispatcher_type: String,
        column_idx: i32,
    }

    impl From<&Dispatcher> for JsonDispatcher {
        fn from(that: &Dispatcher) -> Self {
            JsonDispatcher {
                dispatcher_type: format!("{:?}", that.get_type()),
                column_idx: that.get_column_idx(),
            }
        }
    }

    #[derive(Serialize)]
    pub struct JsonStreamNode {
        input: Vec<JsonStreamNode>,
        pk_indices: Vec<u32>,
        node: JsonNode,
    }

    impl From<&StreamNode> for JsonStreamNode {
        fn from(that: &StreamNode) -> Self {
            JsonStreamNode {
                input: that.get_input().iter().map(JsonStreamNode::from).collect(),
                pk_indices: that.get_pk_indices().clone(),
                node: that.get_node().into(),
            }
        }
    }

    #[derive(Serialize)]
    pub struct JsonNode {
        desc: String,
    }

    impl From<&stream_node::Node> for JsonNode {
        fn from(that: &stream_node::Node) -> Self {
            JsonNode {
                desc: format!("{:#?}", that),
            }
        }
    }

    pub async fn list_actors_inner(srv: Service) -> MetaResult<Vec<JsonActorLocation>> {
        let result = srv
            .stream_meta_manager
            .load_all_actors()
            .await?
            .iter()
            .map(JsonActorLocation::from)
            .collect_vec();
        Ok(result)
    }

    pub async fn list_actors(srv: Service) -> impl warp::Reply {
        match list_actors_inner(srv).await {
            Ok(reply) => {
                warp::reply::with_status(warp::reply::json(&reply), warp::http::StatusCode::OK)
            }
            Err(err) => warp::reply::with_status(
                warp::reply::json(&err),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            ),
        }
    }
}

mod filters {
    use super::*;

    pub fn with_service(
        srv: Service,
    ) -> impl Filter<Extract = (Service,), Error = Infallible> + Clone {
        warp::any().map(move || srv.clone())
    }

    pub fn clusters_list(
        srv: Service,
    ) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        warp::path!("api" / "clusters" / i32)
            .and(warp::get())
            .and(with_service(srv))
            .then(handlers::list_clusters)
    }

    pub fn clusters(
        srv: Service,
    ) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        clusters_list(srv)
    }

    pub fn actors_list(
        srv: Service,
    ) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        warp::path!("api" / "actors")
            .and(warp::get())
            .and(with_service(srv))
            .then(handlers::list_actors)
    }

    pub fn actors(
        srv: Service,
    ) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        actors_list(srv)
    }
}

impl DashboardService {
    pub async fn serve(self) -> Result<()> {
        let srv = Arc::new(self);

        info!("starting dashboard service at {:?}", srv.dashboard_addr);
        let api = filters::clusters(srv.clone()).or(filters::actors(srv.clone()));

        let index = warp::get().and(warp::path::end()).map(|| {
            warp::http::Response::builder()
                .header("content-type", "text/html; charset=utf-8")
                .body(std::str::from_utf8(include_bytes!("index.html")).unwrap())
        });
        warp::serve(index.or(api)).run(srv.dashboard_addr).await;
        Ok(())
    }

    pub async fn add_test_data(self: &Arc<Self>) -> Result<()> {
        use std::sync::atomic::Ordering;
        if self.has_test_data.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.has_test_data.store(true, Ordering::SeqCst);

        // TODO: remove adding test data
        use risingwave_pb::common::HostAddress;
        use risingwave_pb::meta::ClusterType;

        // TODO: remove adding frontend register when frontend implement register.
        self.cluster_manager
            .add_worker_node(
                HostAddress {
                    host: "127.0.0.1".to_string(),
                    port: 4567,
                },
                ClusterType::Frontend,
            )
            .await?;

        Ok(())
    }
}
