use std::collections::HashMap as StdHashMap;
use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::network_base::{Sender, Server, WsConnection};
use business::{self, BUSINESS_CACHE};
use cache::{JointData, NRSC_CACHE};
use catchup;
use composer::*;
use config;
use error::Result;
use failure::ResultExt;
use hashbrown::HashMap;
use joint::{Joint, JointSequence, Level};
use light;
use main_chain;
use may::coroutine;
use may::net::TcpStream;
use may::sync::RwLock;
use notify_watcher;
use rcu_cell::RcuReader;
use nrsc_object_base::object_hash;
use serde_json::{self, Value};
use statistics;
use tungstenite::client::client;
use tungstenite::handshake::client::Request;
use tungstenite::protocol::Role;
use url::Url;
use utils::{AtomicLock, FifoCache, MapLock, OnceOption};
use validation;
use wallet_info::MY_WALLET;

//---------------------------------------------------------------------------------------
// Global Data
//---------------------------------------------------------------------------------------

// global data that record the internal state
lazy_static! {
    // global Ws connections
    pub static ref WSS: WsConnections = WsConnections::new();
    // maybe this is too heavy, could use an optimized hashset<AtomicBool>
    static ref UNIT_IN_WORK: MapLock<String> = MapLock::new();
    static ref JOINT_IN_REQ: MapLock<String> = MapLock::new();
    static ref IS_CATCHING_UP: AtomicLock = AtomicLock::new();
    static ref SELF_LISTEN_ADDRESS: Option<String> = config::get_listen_address();
    static ref BAD_CONNECTION: FifoCache<String, ()> = FifoCache::with_capacity(10);
    static ref UNKNOWN_PEER_ID: Arc<String> = Arc::new(String::from("unknown_peer"));
}

//---------------------------------------------------------------------------------------
// HubNetState
//---------------------------------------------------------------------------------------
#[derive(Serialize, Deserialize)]
pub struct ConnState {
    peer_id: String,
    peer_addr: String,
    is_subscribed: bool,
    listen_addr: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct HubNetState {
    // peer_id, peer_addr, is_source, is_subscribed
    pub in_bounds: Vec<ConnState>,
    pub out_bounds: Vec<ConnState>,
}

//---------------------------------------------------------------------------------------
// WsConnections
//---------------------------------------------------------------------------------------
// global request has no specific ws connections, just find a proper one should be fine
pub struct WsConnections {
    // <peer_id, conn>
    conns: RwLock<HashMap<Arc<String>, Arc<HubConn>>>,
    next_conn: AtomicUsize,
}

impl WsConnections {
    fn new() -> Self {
        WsConnections {
            conns: RwLock::new(HashMap::new()),
            next_conn: AtomicUsize::new(0),
        }
    }

    pub fn add_p2p_conn(&self, conn: Arc<HubConn>, is_inbound: bool) -> Result<()> {
        init_connection(&conn)?;
        if is_inbound {
            conn.set_inbound();
        }
        add_peer_host(&conn)?;
        let peer_id = conn.get_peer_id();
        warn!(
            "add_p2p_conn peer_id={} peer_addr={}",
            peer_id,
            conn.get_peer_addr()
        );
        self.conns.write().unwrap().insert(peer_id, conn);
        Ok(())
    }

    pub fn close_all(&self) {
        let mut g = self.conns.write().unwrap();
        g.clear();
    }

    fn close(&self, conn: &HubConn) {
        // find out the actor and remove it
        let mut g = self.conns.write().unwrap();
        g.remove(&conn.get_peer_id());
    }

    pub fn get_next_peer(&self) -> Option<Arc<HubConn>> {
        let g = self.conns.read().unwrap();
        let mut peers = g.values();
        let len = peers.len();
        if len == 0 {
            return None;
        }

        let idx = self.next_conn.fetch_add(1, Ordering::Relaxed) % len;
        peers.nth(idx).cloned()
    }

    // return all remote peer addresses
    fn get_peers_from_remote(&self) -> Vec<String> {
        let mut peers: Vec<String> = Vec::new();
        let hub_id = Value::from(MY_WALLET._00_address.as_str());

        // only get peers from source connections
        let conns = {
            // need to drop the connection earlier
            let g = self.conns.read().unwrap();
            g.values()
                .filter(|c| c.get_listen_addr().is_some())
                .cloned()
                .collect::<Vec<_>>()
        };

        for conn in conns {
            if let Ok(value) = conn.send_request("get_peers", &hub_id) {
                if let Ok(mut tmp) = serde_json::from_value(value) {
                    peers.append(&mut tmp);
                }
            }
        }

        peers.sort();
        peers.dedup();

        peers
    }

    pub fn get_connection(&self, peer_id: Arc<String>) -> Option<Arc<HubConn>> {
        let g = self.conns.read().unwrap();
        g.get(&peer_id).cloned()
    }

    pub fn broadcast_joint(&self, joint: RcuReader<JointData>) {
        // disable broadcast during catchup
        let _g = match IS_CATCHING_UP.try_lock() {
            Some(g) => g,
            None => return,
        };

        for conn in self.conns.read().unwrap().values().cloned() {
            // only send to who subscribed and not the source
            if conn.is_subscribed() && joint.get_peer_id() != Some(conn.get_peer_id()) {
                let joint = joint.clone();
                try_go!(move || conn.send_joint(&joint));
            }
        }
    }

    /// notify message to watcher
    pub fn notify_watcher(&self, peer_id: Arc<String>, message: Value) -> Result<bool> {
        match self.get_connection(peer_id) {
            Some(conn) => {
                try_go!(move || conn.send_notify(&message));
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn broadcast_free_joint_list(&self, free_units: &[String]) {
        // disable broadcast during catchup
        let _g = match IS_CATCHING_UP.try_lock() {
            Some(g) => g,
            None => return,
        };

        coroutine::scope(|scope| {
            for conn in self.conns.read().unwrap().values().cloned() {
                // only send to who subscribed
                if conn.is_subscribed() {
                    try_go!(scope, move || conn.send_free_joint_list(free_units));
                }
            }
        });
    }

    pub fn request_free_joints_from_all_peers(&self) -> Result<()> {
        for conn in self.conns.read().unwrap().values().cloned() {
            if conn.get_listen_addr().is_some() {
                try_go!(move || conn.send_just_saying("refresh", Value::Null));
            }
        }
        Ok(())
    }

    fn get_outbound_peers(&self, hub_id: &str) -> Vec<ConnState> {
        // filter out the connection with the same hub_id
        self.conns
            .read()
            .unwrap()
            .values()
            .filter(|c| !c.is_inbound() && c.get_peer_id().as_str() != hub_id)
            .map(|c| ConnState {
                peer_id: c.get_peer_id().to_string(),
                peer_addr: c.get_peer_addr().to_string(),
                is_subscribed: c.is_subscribed(),
                listen_addr: c.get_listen_addr(),
            })
            .collect()
    }

    fn get_hub_peers(&self, hub_id: &str) -> Vec<ConnState> {
        // filter out the connection with the same hub_id
        let mut peers = self
            .conns
            .read()
            .unwrap()
            .values()
            .filter(|c| c.get_listen_addr().is_some() && c.get_peer_id().as_str() != hub_id)
            .map(|c| ConnState {
                peer_id: c.get_peer_id().to_string(),
                peer_addr: c.get_peer_addr().to_string(),
                is_subscribed: c.is_subscribed(),
                listen_addr: c.get_listen_addr(),
            })
            .collect::<Vec<_>>();

        // include self listen address
        if let Some(ref addr) = *SELF_LISTEN_ADDRESS {
            peers.push(ConnState {
                peer_id: MY_WALLET._00_address.clone(),
                peer_addr: addr.to_owned(),
                is_subscribed: true,
                listen_addr: Some(addr.to_owned()),
            })
        }

        peers
    }

    fn get_inbound_peers(&self) -> Vec<ConnState> {
        self.conns
            .read()
            .unwrap()
            .values()
            .filter(|c| c.is_inbound())
            .map(|c| ConnState {
                peer_id: c.get_peer_id().to_string(),
                peer_addr: c.get_peer_addr().to_string(),
                is_subscribed: c.is_subscribed(),
                listen_addr: c.get_listen_addr(),
            })
            .collect()
    }

    fn get_net_state(&self) -> HubNetState {
        HubNetState {
            in_bounds: self.get_inbound_peers(),
            out_bounds: self.get_outbound_peers(""),
        }
    }

    fn get_net_statistics(&self) -> StdHashMap<String, statistics::LastConnStat> {
        let mut all_stats = statistics::get_all_last_stats();
        let g = self.conns.read().unwrap();
        for conn in g.keys() {
            if let Some(stat) = all_stats.get_mut(conn.as_str()) {
                stat.is_connected = true;
            }
        }

        all_stats
    }

    fn get_needed_outbound_peers(&self) -> usize {
        let outbound_connecions = self
            .conns
            .read()
            .unwrap()
            .values()
            .filter(|c| !c.is_inbound())
            .count();
        if config::MAX_OUTBOUND_CONNECTIONS > outbound_connecions {
            return config::MAX_OUTBOUND_CONNECTIONS - outbound_connecions;
        }
        0
    }

    fn contains(&self, addr: &str) -> bool {
        if let Some(peer_id) = statistics::get_peer_id_by_address(addr) {
            return self.conns.read().unwrap().contains_key(&peer_id);
        }

        false
    }
}

//---------------------------------------------------------------------------------------
// HubConn
//---------------------------------------------------------------------------------------

pub struct HubData {
    // indicate if this connection is a subscribed peer
    is_subscribed: AtomicBool,
    is_inbound: AtomicBool,
    peer_id: OnceOption<Arc<String>>,
    listen_addr: OnceOption<String>,
}

pub type HubConn = WsConnection<HubData>;

impl Default for HubData {
    fn default() -> Self {
        HubData {
            is_subscribed: AtomicBool::new(false),
            is_inbound: AtomicBool::new(false),
            peer_id: OnceOption::new(),
            listen_addr: OnceOption::new(),
        }
    }
}

impl Server<HubData> for HubData {
    fn on_message(ws: Arc<HubConn>, subject: String, body: Value) -> Result<()> {
        match subject.as_str() {
            "version" => ws.on_version(body)?,
            "error" => error!("receive error: {}", body),
            "info" => info!("receive info: {}", body),
            "result" => info!("receive result: {}", body),
            "joint" => ws.on_joint(body)?,
            "refresh" => ws.on_refresh(body)?,
            "light/new_address_to_watch" => ws.on_new_address_to_watch(body)?,
            "free_joint_list" => ws.on_free_joint_list(body)?,

            subject => bail!(
                "on_message unknown subject: {} body {}",
                subject,
                body.to_string()
            ),
        }
        Ok(())
    }

    fn on_request(ws: Arc<HubConn>, command: String, params: Value) -> Result<Value> {
        let response = match command.as_str() {
            "heartbeat" => ws.on_heartbeat(params)?,
            "subscribe" => ws.on_subscribe(params)?,
            "catchup" => ws.on_catchup(params)?,
            "post_joint" => ws.on_post_joint(params)?,
            "net_state" => ws.on_get_net_state(params)?,
            "net_statistics" => ws.on_get_net_statistics(params)?,
            "light/inputs" => ws.on_get_inputs(params)?,
            "light/get_history" => ws.on_get_history(params)?,
            "light/light_props" => ws.on_get_light_props(params)?,
            "light/get_link_proofs" => ws.on_get_link_proofs(params)?,
            "get_joint" => ws.on_get_joint(params)?,
            "get_peers" => ws.on_get_peers(params)?,
            "get_text" => ws.on_get_text(params)?,
            "get_balance" => ws.on_get_balance(params)?,
            "get_hash_tree" => ws.on_get_hash_tree(params)?,
            "get_witnesses" => ws.on_get_witnesses(params)?,
            "get_free_joints" => ws.on_get_free_joints(params)?,
            "get_joints_info" => ws.on_get_joints_info(params)?,
            "get_network_info" => ws.on_get_network_info(params)?,
            "get_joints_by_mci" => ws.on_get_joints_by_mci(params)?,
            "get_missing_joints" => ws.on_get_missing_joints(params)?,
            "get_bad_joints" => ws.on_get_bad_joints(params)?,
            "get_temp_bad_joints" => ws.on_get_temp_bad_joints(params)?,
            "get_joints_by_level" => ws.on_get_joints_by_level(params)?,
            "get_joint_by_unit_hash" => ws.on_get_joint_by_unit_hash(params)?,
            "get_children" => ws.on_get_children(params)?,
            "get_tps" => ws.on_get_tps(params)?,
            "watch" => ws.on_watch(params)?,

            command => bail!("on_request unknown command: {}", command),
        };
        Ok(response)
    }

    fn close(ws: Arc<HubConn>) {
        ws.close()
    }
}

// internal state access
impl HubConn {
    pub fn is_subscribed(&self) -> bool {
        let data = self.get_data();
        data.is_subscribed.load(Ordering::Relaxed)
    }

    fn set_subscribed(&self) {
        let data = self.get_data();
        data.is_subscribed.store(true, Ordering::Relaxed);
    }

    pub fn is_inbound(&self) -> bool {
        let data = self.get_data();
        data.is_inbound.load(Ordering::Relaxed)
    }

    pub fn set_inbound(&self) {
        let data = self.get_data();
        data.is_inbound.store(true, Ordering::Relaxed);
    }

    pub fn get_peer_id(&self) -> Arc<String> {
        let data = self.get_data();
        data.peer_id
            .get()
            .cloned()
            .unwrap_or_else(|| UNKNOWN_PEER_ID.clone())
    }

    pub fn set_peer_id(&self, peer_id: &str) {
        let data = self.get_data();
        data.peer_id.set(Arc::new(peer_id.to_owned()));
    }

    pub fn get_listen_addr(&self) -> Option<String> {
        let data = self.get_data();
        data.listen_addr.get().cloned()
    }

    pub fn set_listen_addr(&self, listen_addr: Option<String>) {
        let data = self.get_data();
        if let Some(addr) = listen_addr {
            data.listen_addr.set(addr);
        }
    }
}

// the server side impl
impl HubConn {
    fn on_version(&self, version: Value) -> Result<()> {
        if version["protocol_version"].as_str() != Some(config::VERSION) {
            error!("Incompatible versions, mine {}", config::VERSION);
            self.close();
        }

        if version["alt"].as_str() != Some(config::ALT) {
            error!("Incompatible alt, mine {}", config::ALT);
            self.close();
        }

        info!("got peer version: {}", version);
        Ok(())
    }

    fn on_get_balance(&self, param: Value) -> Result<Value> {
        let addr = param
            .as_str()
            .ok_or_else(|| format_err!("no address for get_balance"))?;
        let balance = BUSINESS_CACHE.global_state.get_stable_balance(addr)?;

        Ok(json!({"address": addr, "balance": balance}))
    }

    fn on_get_text(&self, param: Value) -> Result<Value> {
        let unit = param.as_str().ok_or_else(|| format_err!("wrong address"))?;

        Ok(json!(business::text::get_text(unit)?))
    }

    fn on_get_tps(&self, _param: Value) -> Result<Value> {
        let tps_info = statistics::get_tps_info();

        Ok(serde_json::to_value(tps_info)?)
    }

    fn on_get_inputs(&self, param: Value) -> Result<Value> {
        let inputs_request: light::InputsRequest = serde_json::from_value(param)?;

        let ret = light::get_inputs_for_amount(inputs_request)?;

        Ok(serde_json::to_value(ret)?)
    }

    fn on_get_joints_info(&self, _param: Value) -> Result<Value> {
        Ok(json!(light::NumOfUnit {
            valid_unit: NRSC_CACHE.get_num_of_normal_joints(),
            known_bad: NRSC_CACHE.get_num_of_bad_joints(),
            temp_bad: NRSC_CACHE.get_num_of_temp_bad_joints(),
            unhandled: NRSC_CACHE.get_num_of_unhandled_joints(),
            last_stable_mci: main_chain::get_last_stable_mci(),
        }))
    }

    fn on_get_light_props(&self, param: Value) -> Result<Value> {
        if !self.is_inbound() {
            bail!("light clients have to be inbound");
        }
        let address: String = serde_json::from_value(param)?;
        let ParentsAndLastBall {
            parents,
            last_ball,
            last_ball_unit,
        } = pick_parents_and_last_ball(&address)?;

        let light_props = light::LightProps {
            last_ball,
            last_ball_unit,
            parent_units: parents,
            witness_list_unit: ::spec::GENESIS_UNIT.to_string(),
            has_definition: NRSC_CACHE.get_definition(&address).is_some(),
        };

        Ok(serde_json::to_value(light_props)?)
    }

    fn on_heartbeat(&self, _: Value) -> Result<Value> {
        Ok(Value::Null)
    }

    fn on_subscribe(&self, param: Value) -> Result<Value> {
        let peer_id = param["peer_id"]
            .as_str()
            .ok_or_else(|| format_err!("no peer_id"))?;
        if peer_id == MY_WALLET._00_address {
            self.close();
            bail!("self-connect");
        }

        info!(
            "on_subscribe peer_id={}, peer_addr={}",
            peer_id,
            self.get_peer_addr()
        );
        self.set_subscribed();
        self.set_peer_id(peer_id);
        if let Some(ws) = WSS.get_connection(self.get_peer_id()) {
            let peer_addr = ws.get_peer_addr();
            if peer_addr != self.get_peer_addr() {
                // we already have an outbound connection with the same peer_id
                bail!(
                    "peer_id={} already connected, old_addr={}",
                    peer_id,
                    peer_addr,
                );
            }
        }

        // get listen address
        let listen_addr = param["listen_addr"].as_str();
        self.set_listen_addr(listen_addr.map(|s| s.to_owned()));

        // send some joint in a background task
        let last_mci = param["last_mci"].as_u64();
        let peer_id = self.get_peer_id();
        try_go!(move || -> Result<()> {
            // wait connection init done
            ::utils::wait_cond(None, || WSS.get_connection(peer_id.clone()).is_some())?;
            let ws = WSS
                .get_connection(peer_id)
                .ok_or_else(|| format_err!("connection not init done yet"))?;
            if let Some(last_mci) = last_mci {
                ws.send_joints_since_mci(Level::from(last_mci as usize))?;
            } else {
                // send genesis unit
                let genesis = NRSC_CACHE.get_joint(&::spec::GENESIS_UNIT)?.read()?;
                ws.send_joint(&*genesis)?;
            }
            ws.send_free_joints()?;
            Ok(())
        });

        Ok(json!({
            "peer_id": MY_WALLET._00_address,
            "listen_addr": *SELF_LISTEN_ADDRESS
        }))
    }

    fn on_get_joint(&self, param: Value) -> Result<Value> {
        let unit: String = serde_json::from_value(param)?;

        match NRSC_CACHE.get_joint(&unit).and_then(|j| j.read()) {
            Ok(joint) => {
                statistics::increase_stats(self.get_peer_id(), false, true);

                Ok(json!({ "joint": clear_ball_after_min_retrievable_mci(&joint)?}))
            }

            Err(e) => {
                error!(
                    "read joint {} failed, err={}, peer_addr={}",
                    unit,
                    e,
                    self.get_peer_addr()
                );

                Ok(json!({ "joint_not_found": unit }))
            }
        }
    }

    fn on_get_free_joints(&self, _param: Value) -> Result<Value> {
        match NRSC_CACHE.get_good_free_joints() {
            Ok(mut joints) => {
                joints.sort_unstable_by(|a, b| {
                    b.read()
                        .unwrap()
                        .get_level()
                        .value()
                        .cmp(&a.read().unwrap().get_level().value())
                });

                Ok(json!(joints
                    .iter()
                    .map(|v| v.key.to_string())
                    .collect::<Vec<String>>()))
            }

            Err(e) => {
                error!(" err={}", e);
                bail!("{}", e);
            }
        }
    }

    fn on_get_missing_joints(&self, _param: Value) -> Result<Value> {
        let joints = NRSC_CACHE.get_missing_joints();
        Ok(json!(joints))
    }

    fn on_joint(&self, param: Value) -> Result<()> {
        let joint: Joint = serde_json::from_value(param)?;
        info!("receive a joint: {:?}", joint);
        ensure!(!joint.unit.unit.is_empty(), "no unit");
        self.handle_online_joint(joint, false)
    }

    fn on_catchup(&self, param: Value) -> Result<Value> {
        let catchup_req: catchup::CatchupReq = serde_json::from_value(param)?;
        let catchup_chain = catchup::prepare_catchup_chain(catchup_req)?;
        Ok(serde_json::to_value(catchup_chain)?)
    }

    fn on_get_hash_tree(&self, param: Value) -> Result<Value> {
        let hash_tree_req: catchup::HashTreeReq = serde_json::from_value(param)?;
        let hash_tree = catchup::prepare_hash_tree(hash_tree_req)?;
        Ok(json!({ "balls": hash_tree }))
    }

    fn on_refresh(&self, param: Value) -> Result<()> {
        let _g = match IS_CATCHING_UP.try_lock() {
            Some(g) => g,
            None => return Ok(()),
        };

        let mci = param.as_u64();
        if let Some(mci) = mci {
            self.send_joints_since_mci(Level::from(mci as usize))?;
        }
        self.send_free_joints()?;

        Ok(())
    }

    fn on_new_address_to_watch(&self, param: Value) -> Result<()> {
        if !self.is_inbound() {
            return self.send_error(Value::from("light clients have to be inbound"));
        }

        let address: String = serde_json::from_value(param).context("not an address string")?;
        if !object_hash::is_chash_valid(&address) {
            return self.send_error(Value::from("address not valid"));
        }

        // TODO: client should report it's interested address
        unimplemented!()
    }

    fn on_get_peers(&self, param: Value) -> Result<Value> {
        let peer_id = param.as_str();
        let peers = WSS.get_hub_peers(peer_id.unwrap_or("unknown"));
        let peer_addrs = peers
            .into_iter()
            .filter_map(|p| {
                p.listen_addr.and_then(|addr| {
                    if addr.starts_with("127.0.0") {
                        None
                    } else {
                        Some(addr)
                    }
                })
            })
            .collect::<Vec<String>>();

        Ok(serde_json::to_value(peer_addrs)?)
    }

    fn on_get_net_state(&self, _param: Value) -> Result<Value> {
        let net_state = WSS.get_net_state();
        Ok(serde_json::to_value(net_state)?)
    }

    fn on_get_net_statistics(&self, _param: Value) -> Result<Value> {
        let net_stats = WSS.get_net_statistics();
        Ok(serde_json::to_value(net_stats)?)
    }

    fn on_get_witnesses(&self, _: Value) -> Result<Value> {
        use my_witness::MY_WITNESSES;
        Ok(serde_json::to_value(&*MY_WITNESSES)?)
    }

    /// get free joint list from peers, request my lost free joints
    fn on_free_joint_list(&self, param: Value) -> Result<()> {
        // disable broadcast during catchup
        let _g = match IS_CATCHING_UP.try_lock() {
            Some(g) => g,
            None => return Ok(()),
        };

        let free_units: Vec<String> =
            serde_json::from_value(param).context("failed to parse free list")?;
        let mut lost_frees = Vec::new();
        for unit in free_units {
            // if my normal/ unhandle/ known bad joints all have no the unit, means I lost the unit
            if NRSC_CACHE.check_new_joint(&unit).is_ok() {
                lost_frees.push(unit);
            }
        }

        if !lost_frees.is_empty() {
            self.request_joints(lost_frees)?;
        }

        Ok(())
    }

    fn on_post_joint(&self, param: Value) -> Result<Value> {
        let joint: Joint = serde_json::from_value(param)?;
        info!("receive a posted joint: {:?}", joint);

        self.handle_online_joint(joint, true)?;

        Ok(Value::from("accepted"))
    }

    fn on_get_history(&self, param: Value) -> Result<Value> {
        if !self.is_inbound() {
            bail!("light clients have to be inbound");
        }

        let history_request: light::HistoryRequest = serde_json::from_value(param)?;

        let ret = light::get_latest_history(&history_request)?;

        Ok(serde_json::to_value(ret)?)
    }

    fn on_get_link_proofs(&self, _params: Value) -> Result<Value> {
        if !self.is_inbound() {
            bail!("light clients have to be inbound");
        }
        // let units: Vec<String> =
        //     serde_json::from_value(params).context("prepare_Link_proofs.params is error")?;
        // Ok(serde_json::to_value(light::prepare_link_proofs(
        //     &units,
        // )?)?)
        Ok(json![null])
    }

    fn on_get_network_info(&self, _param: Value) -> Result<Value> {
        let version = config::VERSION;
        let peers = WSS.get_inbound_peers().len();
        let tps = 1050;
        let last_mci = main_chain::get_last_stable_mci().value();
        let total_units = NRSC_CACHE.get_joints_len();

        Ok(json!({
            "version": version,
            "peers": peers,
            "tps": tps,
            "last_mci": last_mci,
            "total_units": total_units,
        }))
    }

    fn on_get_joints_by_mci(&self, param: Value) -> Result<Value> {
        let mci = param
            .as_i64()
            .ok_or_else(|| format_err!("not a valid mci"))?;

        let joints: Vec<Joint> = if mci < 0 {
            NRSC_CACHE
                .get_unstable_joints()?
                .into_iter()
                .map(|j| j.read())
                // Skip those failed to read
                .filter(|j| j.is_ok())
                .map(|j| (**j.unwrap()).clone())
                .collect()
        } else {
            NRSC_CACHE
                .get_joints_by_mci(Level::from(mci as usize))?
                .into_iter()
                .map(|j| j.read())
                // Skip those failed to read
                .filter(|j| j.is_ok())
                .map(|j| (**j.unwrap()).clone())
                .collect()
        };

        Ok(json!({ "joints": joints }))
    }

    fn on_get_joint_by_unit_hash(&self, param: Value) -> Result<Value> {
        let unit: String = serde_json::from_value(param)?;

        NRSC_CACHE
            .get_joint(&unit)
            .and_then(|j| j.read())
            .and_then(|j| {
                Ok(json!({
                    "joint": (**j).clone(),
                    "property": &*j.get_all_props().read().unwrap()
                }))
            })
    }

    fn on_get_joints_by_level(&self, param: Value) -> Result<Value> {
        let min = param["min_level"]
            .as_u64()
            .ok_or_else(|| format_err!("min_level not in param"))?;
        let max = param["max_level"]
            .as_u64()
            .ok_or_else(|| format_err!("max_level not in param"))?;

        let min_level = ::std::cmp::min(min, max) as usize;
        let max_level = ::std::cmp::max(min, max) as usize;

        let units = ::explore::get_joints_by_level(min_level.into(), max_level.into())?;

        let ret = serde_json::to_value(units)?;

        Ok(ret)
    }

    fn on_get_bad_joints(&self, _param: Value) -> Result<Value> {
        Ok(serde_json::to_value(NRSC_CACHE.get_bad_joints())?)
    }

    fn on_get_temp_bad_joints(&self, _param: Value) -> Result<Value> {
        Ok(serde_json::to_value(NRSC_CACHE.get_temp_bad_joints())?)
    }

    fn on_get_children(&self, param: Value) -> Result<Value> {
        let unit: String = serde_json::from_value(param)?;

        let joint = NRSC_CACHE.get_joint(&unit)?.read()?;
        let children = joint
            .children
            .iter()
            .map(|c| c.key.to_string())
            .collect::<Vec<_>>();

        Ok(serde_json::to_value(children)?)
    }

    fn on_watch(&self, param: Value) -> Result<Value> {
        let watch_addresses: Vec<String> = serde_json::from_value(param)?;
        notify_watcher::watcher_insert(&self.get_peer_id(), &watch_addresses);

        Ok(Value::Null)
    }
}

impl HubConn {
    fn handle_online_joint(&self, joint: Joint, is_post: bool) -> Result<()> {
        // clear the main chain index, main chain index is used by light only
        // joint.unit.main_chain_index = None;

        // check content_hash or unit_hash first!
        validation::validate_unit_hash(&joint.unit)?;

        // check if unit is in work, when g is dropped unlock the unit
        let g = UNIT_IN_WORK.try_lock(vec![joint.unit.unit.to_owned()]);
        if g.is_none() {
            // the unit is in work, do nothing
            return Ok(());
        }

        let cached_joint = match NRSC_CACHE.add_new_joint(joint, Some(self.get_peer_id())) {
            Ok(j) => j,
            Err(e) => {
                warn!("add_new_joint: {}", e);
                return Ok(());
            }
        };
        let joint_data = cached_joint.read().unwrap();
        joint_data.set_is_post(is_post);

        if let Some(ref hash) = joint_data.unit.content_hash {
            error!("unit {} content hash = {}", cached_joint.key, hash);
            joint_data.set_sequence(JointSequence::FinalBad);
        }

        if joint_data.is_ready() {
            return validation::validate_ready_joint(cached_joint);
        }

        // trigger catchup
        if let Some(ball) = &joint_data.ball {
            if !NRSC_CACHE.is_ball_in_hash_tree(ball) {
                // need to catchup and keep the joint in unhandled till timeout
                let ws = WSS.get_connection(self.get_peer_id()).unwrap();
                go!(move || {
                    // if we already in catchup mode, just return
                    let _g = match IS_CATCHING_UP.try_lock() {
                        Some(g) => g,
                        None => return,
                    };

                    if let Err(e) = start_catchup(ws) {
                        error!("catchup failed, err={}", e);
                    }
                    // after the catchup done, clear the hash tree ball
                    NRSC_CACHE.clear_hash_tree_ball();
                });
                return Ok(());
            }
        } else {
            // missing parent, ask for them
            let missing_parents = joint_data.get_missing_parents()?;
            self.request_new_missing_joints(missing_parents)?;
        }
        Ok(())
    }

    // record peer event in database
    #[allow(dead_code)]
    fn write_event(&self, _event: &str) -> Result<()> {
        // TODO: record peer event
        // if event.contains("invalid") || event.contains("nonserial") {
        //     let host = self.get_peer_addr();
        //     let event_string: String = event.to_string();
        //     let column = format!("count_{}_joints", &event_string);
        //     let sql = format!(
        //         "UPDATE peer_hosts SET {}={}+1 WHERE peer_host=?",
        //         column, column
        //     );
        //     let mut stmt = db.prepare_cached(&sql)?;
        //     stmt.execute(&[host])?;

        //     let mut stmt =
        //         db.prepare_cached("INSERT INTO peer_events (peer_host, event) VALUES (?, ?)")?;
        //     stmt.execute(&[host, &event_string])?;
        // }

        Ok(())
    }

    fn request_catchup(&self) -> Result<Vec<String>> {
        info!("will request catchup from {}", self.get_peer_addr());

        // here we send out the real catchup request
        let last_stable_mci = main_chain::get_last_stable_mci();
        let witnesses = &*::my_witness::MY_WITNESSES;
        let param = json!({
            "witnesses": witnesses,
            "last_stable_mci": last_stable_mci.value(),
            "last_known_mci": last_stable_mci.value()
        });

        let ret = self.send_request("catchup", &param)?;
        if !ret["error"].is_null() {
            bail!("catchup request got error response: {:?}", ret["error"]);
        }

        let catchup_chain: catchup::CatchupChain = serde_json::from_value(ret)?;
        catchup::process_catchup_chain(catchup_chain)
    }

    fn request_new_missing_joints<'a>(
        &self,
        units: impl Iterator<Item = &'a String>,
    ) -> Result<()> {
        let mut new_units = Vec::new();

        for unit in units {
            let g = UNIT_IN_WORK.try_lock(vec![unit.clone()]);
            if g.is_none() {
                // other thread is working on the unit, skip it
                debug!("request unit in working. unit={}", unit);
                continue;
            }

            // re-check if this is necessary
            if let Err(e) = NRSC_CACHE.check_new_joint(unit) {
                info!("unnecessary request unit: {} ", e);
                continue;
            }

            new_units.push(unit.clone());
        }

        self.request_joints(new_units)?;
        Ok(())
    }

    fn request_next_hash_tree(
        &self,
        from_ball: &str,
        to_ball: &str,
    ) -> Result<Vec<catchup::BallProps>> {
        let mut hash_tree = self.send_request(
            "get_hash_tree",
            &json!({
                "from_ball": from_ball,
                "to_ball": to_ball,
            }),
        )?;

        if !hash_tree["error"].is_null() {
            bail!("get_hash_tree got error response: {}", hash_tree["error"]);
        }

        Ok(serde_json::from_value(hash_tree["balls"].take())?)
    }

    #[inline]
    fn send_joint(&self, joint: &Joint) -> Result<()> {
        statistics::increase_stats(self.get_peer_id(), false, true);

        self.send_just_saying("joint", serde_json::to_value(joint)?)
    }

    fn send_free_joint_list(&self, free_units: &[String]) -> Result<()> {
        self.send_just_saying("free_joint_list", serde_json::to_value(free_units)?)
    }

    /// send notify message to watcher
    fn send_notify(&self, value: &Value) -> Result<()> {
        self.send_just_saying("notify", value.to_owned())
    }

    /// send stable joints to trigger peer catchup
    fn send_joints_since_mci(&self, mci: Level) -> Result<()> {
        let last_stable_mci = main_chain::get_last_stable_mci();
        // peer no need catchup
        if mci >= last_stable_mci {
            return Ok(());
        }

        if mci <= Level::ZERO {
            // send genesis unit first to define the witnesses
            let genesis = NRSC_CACHE.get_joint(&::spec::GENESIS_UNIT)?.read()?;
            self.send_joint(&*genesis)?;
        }

        // only send latest stable joints
        for joint in NRSC_CACHE.get_joints_by_mci(last_stable_mci)? {
            self.send_joint(&clear_ball_after_min_retrievable_mci(&*joint.read()?)?)?;
        }

        Ok(())
    }

    fn send_free_joints(&self) -> Result<()> {
        let joints = NRSC_CACHE.get_good_free_joints()?;
        for joint in joints {
            let joint = joint.read()?;
            self.send_joint(&**joint)?;
        }
        Ok(())
    }
}

// the client side impl
impl HubConn {
    fn send_version(&self) -> Result<()> {
        self.send_just_saying(
            "version",
            json!({
                "protocol_version": config::VERSION,
                "alt": config::ALT,
                "library": config::LIBRARY,
                "library_version": config::LIBRARY_VERSION,
                "program": "rust-nrsc-hub",
                // TODO: read from Cargo.toml
                "program_version": "0.1.0"
            }),
        )
    }

    fn send_subscribe(&self) -> Result<()> {
        let last_mci = main_chain::get_last_stable_mci();

        match self.send_request(
            "subscribe",
            &json!({ "peer_id": MY_WALLET._00_address,
              "last_mci": last_mci.value(),
              "listen_addr": *SELF_LISTEN_ADDRESS,
            }),
        ) {
            Ok(value) => {
                // the peer id may be ready set in on_subscribe
                // the light client peer_id is the return value
                match value["peer_id"].as_str() {
                    Some(peer_id) => {
                        if self.get_peer_id() == *UNKNOWN_PEER_ID {
                            self.set_peer_id(peer_id);
                        }
                    }
                    // the client must send it peer id back
                    None => bail!("no peer_id set in response of subscribe"),
                }

                // if has listen address
                let listen_addr = value["listen_addr"].as_str();
                self.set_listen_addr(listen_addr.map(|s| s.to_owned()));
            }
            Err(e) => {
                // save the peer address to avoid connect to it again
                BAD_CONNECTION.insert(self.get_peer_addr().to_string(), ());
                bail!(
                    "send subscribe failed, err={}, peer={}",
                    e,
                    self.get_peer_addr()
                );
            }
        }

        Ok(())
    }

    fn send_heartbeat(&self) -> Result<()> {
        self.send_request("heartbeat", &Value::Null)?;
        Ok(())
    }

    pub fn post_joint(&self, joint: &Joint) -> Result<()> {
        self.send_request("post_joint", &serde_json::to_value(joint)?)?;
        Ok(())
    }

    // remove self from global
    fn close(&self) {
        info!("close connection: {}", self.get_peer_addr());
        // we hope that when all related joints are resolved
        // the connection could drop automatically
        WSS.close(self);
    }

    fn request_joints(&self, units: impl IntoIterator<Item = String>) -> Result<()> {
        fn request_joint(ws: Arc<HubConn>, unit: &str) -> Result<()> {
            // if the joint is in request, just ignore
            let g = JOINT_IN_REQ.try_lock(vec![unit.to_owned()]);
            if g.is_none() {
                debug!("already request_joint: {}", unit);
                return Ok(());
            }

            let mut v = ws.send_request("get_joint", &Value::from(unit))?;
            if v["joint_not_found"].as_str() == Some(&unit) {
                // TODO: if self connection failed to request joint, should
                // let available ws to try a again here. see #72
                bail!(
                    "unit {} not found with the connection: {}",
                    unit,
                    ws.get_peer_addr()
                );
            }

            let joint: Joint = serde_json::from_value(v["joint"].take())?;
            info!("receive a requested joint: {:?}", joint);

            if joint.unit.unit != unit {
                let err = format!("I didn't request this unit from you: {}", joint.unit.unit);
                return ws.send_error(Value::from(err));
            }
            drop(g);

            ws.handle_online_joint(joint, false)
        }

        let ws = WSS.get_connection(self.get_peer_id()).ok_or_else(|| {
            format_err!("failed to find connection, peer_id={}", self.get_peer_id())
        })?;

        for unit in units {
            let ws = ws.clone();
            try_go!(move || request_joint(ws, &unit));
        }
        Ok(())
    }

    pub fn get_witnesses(&self) -> Result<Vec<String>> {
        let witnesses = self
            .send_request("get_witnesses", &Value::Null)
            .context("failed to get witnesses")?;
        let witnesses: Vec<String> =
            serde_json::from_value(witnesses).context("failed to parse witnesses")?;
        if witnesses.len() != config::COUNT_WITNESSES {
            bail!(
                "witnesses must contains {} addresses, but we got {}",
                config::COUNT_WITNESSES,
                witnesses.len()
            );
        }
        Ok(witnesses)
    }
}

//---------------------------------------------------------------------------------------
// Global Functions
//---------------------------------------------------------------------------------------

/// timely broadcast the good free units in case they are not send out successfully
pub fn broadcast_free_joint_list() {
    if let Ok(free_joints) = NRSC_CACHE.get_good_free_joints() {
        let free_units: Vec<String> = free_joints.iter().map(|v| v.key.to_string()).collect();
        WSS.broadcast_free_joint_list(&free_units)
    }
}

pub fn auto_connection() {
    let mut counts = WSS.get_needed_outbound_peers();
    if counts == 0 {
        return;
    }

    let peers = get_unconnected_peers_in_config();
    for peer in peers {
        match create_outbound_conn(&peer) {
            Ok(_) => {
                counts -= 1;
                if counts == 0 {
                    return;
                }
            }
            Err(e) => error!("failed to connect to config peer={}, err={}", peer, e),
        }
    }

    let peers = get_unconnected_remote_peers();
    for peer in peers {
        if BAD_CONNECTION.get(&peer).is_some() {
            continue;
        }
        match create_outbound_conn(&peer) {
            Ok(_) => {
                counts -= 1;
                if counts == 0 {
                    return;
                }
            }
            Err(e) => error!("failed to connect to remote peer={}, err={}", peer, e),
        }
    }

    let peers = get_unconnected_peers_in_db();
    for peer in peers {
        if BAD_CONNECTION.get(&peer).is_some() {
            continue;
        }
        match create_outbound_conn(&peer) {
            Ok(_) => {
                counts -= 1;
                if counts == 0 {
                    return;
                }
            }
            Err(e) => error!("failed to connect to db peer={}, err={}", peer, e),
        }
    }
}

pub fn create_outbound_conn<A: ToSocketAddrs>(address: A) -> Result<Arc<HubConn>> {
    let stream = TcpStream::connect(address)?;
    let peer = match stream.peer_addr() {
        Ok(addr) => addr.to_string(),
        Err(_) => "unknown peer".to_owned(),
    };
    let url = Url::parse("wss://localhost/")?;
    let req = Request::from(url);
    let (conn, _) = client(req, stream)?;

    let ws = WsConnection::new(conn, HubData::default(), peer, Role::Client)?;

    WSS.add_p2p_conn(ws.clone(), false)?;
    Ok(ws)
}

/// remove those long time not ready joints
pub fn purge_junk_unhandled_joints(timeout: u64) {
    let now = crate::time::now();

    // maybe we are catching up the missing parents
    let _g = match IS_CATCHING_UP.try_lock() {
        Some(g) => g,
        None => return,
    };

    // remove those joints that stay in unhandled more that 10min
    NRSC_CACHE.purge_old_unhandled_joints(now, timeout);
}

/// remove those long time temp-bad free joints
pub fn purge_temp_bad_free_joints(timeout: u64) -> Result<()> {
    let now = crate::time::now();
    NRSC_CACHE.purge_old_temp_bad_free_joints(now, timeout)
}

/// this fn will be called every 8s in a timer
pub fn re_request_lost_joints() -> Result<()> {
    let _g = match IS_CATCHING_UP.try_lock() {
        Some(g) => g,
        None => {
            warn!("in catching up");
            return Ok(());
        }
    };

    let units = NRSC_CACHE.get_missing_joints();
    if units.is_empty() {
        return Ok(());
    }

    let ws = match WSS.get_next_peer() {
        None => bail!("failed to find next peer"),
        Some(c) => c,
    };
    info!("found next peer {}", ws.get_peer_addr());

    // this is not an atomic operation, but it's fine to request the unit in working
    let new_units = units
        .into_iter()
        .filter(|x| UNIT_IN_WORK.try_lock(vec![(*x).to_owned()]).is_some())
        .collect::<Vec<_>>();

    info!("lost units {:?}", new_units);

    ws.request_joints(new_units)
}

pub fn notify_watchers_about_stable_joints(mci: Level) -> Result<()> {
    use joint::WRITER_MUTEX;
    // the event was emitted from inside mysql transaction, make sure it completes so that the changes are visible
    // If the mci became stable in determineIfStableInLaterUnitsAndUpdateStableMcFlag (rare), write lock is released before the validation commits,
    // so we might not see this mci as stable yet. Hopefully, it'll complete before light/have_updates roundtrip
    let g = WRITER_MUTEX.lock().unwrap();
    // we don't need to block writes, we requested the lock just to wait that the current write completes
    drop(g);
    info!("notify_watchers_about_stable_joints, mci={:?} ", mci);
    if mci.value() <= 1 {
        return Ok(());
    }

    let last_ball_mci = NRSC_CACHE.get_last_ball_mci_of_mci(mci)?;
    let prev_last_ball_mci = NRSC_CACHE.get_last_ball_mci_of_mci((mci.value() - 1).into())?;

    if last_ball_mci == prev_last_ball_mci {
        return Ok(());
    }

    notify_light_clients_about_stable_joints(prev_last_ball_mci, last_ball_mci)
}

fn init_connection(ws: &Arc<HubConn>) -> Result<()> {
    use rand::{thread_rng, Rng};

    // wait for some time for server ready
    coroutine::sleep(Duration::from_millis(1));

    ws.send_version()?;
    ws.send_subscribe()?;

    let mut rng = thread_rng();
    let n: u64 = rng.gen_range(0, 1000);
    let ws_c = Arc::downgrade(ws);

    // start the heartbeat timer for each connection
    go!(move || loop {
        coroutine::sleep(Duration::from_millis(3000 + n));
        let ws = match ws_c.upgrade() {
            Some(ws) => ws,
            None => return,
        };
        if ws.get_last_recv_tm().elapsed() < Duration::from_secs(5) {
            continue;
        }
        // heartbeat failed so just close the connection
        let rsp = ws.send_heartbeat();
        if rsp.is_err() {
            error!("heartbeat err= {}", rsp.unwrap_err());
            ws.close();
            return;
        }
    });

    Ok(())
}

fn add_peer_host(_bound: &HubConn) -> Result<()> {
    // TODO: impl save peer host to database
    Ok(())
}

fn get_unconnected_remote_peers() -> Vec<String> {
    WSS.get_peers_from_remote()
        .into_iter()
        .filter(|peer| !WSS.contains(peer))
        .collect::<Vec<_>>()
}

fn get_unconnected_peers_in_config() -> Vec<String> {
    config::get_remote_hub_url()
        .into_iter()
        .filter(|peer| !WSS.contains(peer))
        .collect::<Vec<_>>()
}

fn get_unconnected_peers_in_db() -> Vec<String> {
    // TODO: impl
    Vec::new()
}

fn start_catchup(ws: Arc<HubConn>) -> Result<()> {
    info!("catchup started");

    // before a catchup the hash_tree_ball should be clear
    assert_eq!(NRSC_CACHE.get_hash_tree_ball_len(), 0);
    let mut catchup_chain_balls = ws.request_catchup()?;
    catchup_chain_balls.reverse();

    for batch in catchup_chain_balls.windows(2) {
        let start = batch[0].clone();
        let end = batch[1].clone();

        let batch_balls = ws.request_next_hash_tree(&start, &end)?;

        // check last ball is next item
        if batch_balls.last().map(|p| &p.ball) != Some(&end) {
            bail!("batch last ball not match to ball!");
        }
        catchup::process_hash_tree(&batch_balls)?;

        ws.request_new_missing_joints(batch_balls.iter().map(|j| &j.unit))?;

        // wait the batch number below a value and then start another batch
        ::utils::wait_cond(Some(Duration::from_secs(10)), || {
            NRSC_CACHE.get_hash_tree_ball_len() < 1000
        })
        .context("catchup wait hash tree batch timeout")?;
    }

    // wait all the catchup done
    ::utils::wait_cond(Some(Duration::from_secs(10)), || {
        NRSC_CACHE.get_hash_tree_ball_len() == 0
    })
    .context("catchup wait last ball timeout")?;
    info!("catchup done");

    // wait until there is no more working
    ::utils::wait_cond(None, || UNIT_IN_WORK.get_waiter_num() == 0).ok();

    WSS.request_free_joints_from_all_peers()?;

    Ok(())
}

#[allow(dead_code)]
fn notify_watchers(joint: &Joint) -> Result<()> {
    let unit = &joint.unit;

    // already stable, light clients will require a proof
    if joint.ball.is_some() {
        return Ok(());
    }

    let mut addresses = unit.authors.iter().map(|a| &a.address).collect::<Vec<_>>();
    for message in &unit.messages {
        use spec::Payload;
        if message.app != "payment" || message.payload.is_none() {
            continue;
        }
        match message.payload {
            Some(Payload::Payment(ref payment)) => {
                for output in &payment.outputs {
                    let address = &output.address;
                    if !addresses.contains(&address) {
                        addresses.push(address);
                    }
                }
            }
            _ => unreachable!("payload should be a payment"),
        }
    }

    // TODO: find out peers and send the message to them
    // light clients need timestamp
    let mut joint = joint.clone();
    joint.unit.timestamp = Some(::time::now() / 1000);

    let peer_id = Arc::new(String::from("interested_id"));
    if let Some(ws) = WSS.get_connection(peer_id) {
        ws.send_joint(&joint)?;
    }

    Ok(())
}

fn notify_light_clients_about_stable_joints(_from_mci: Level, _to_mci: Level) -> Result<()> {
    unimplemented!()
}

fn clear_ball_after_min_retrievable_mci(joint_data: &JointData) -> Result<Joint> {
    let mut joint = (**joint_data).clone();

    // min_retrievable mci is the mci of the last ball of the last stable joint
    if joint_data.get_mci()
        >= NRSC_CACHE
            .get_last_ball_mci_of_mci(::main_chain::get_last_stable_mci())
            .unwrap_or(Level::INVALID)
    {
        joint.ball = None;
        joint.skiplist_units = Vec::new();
    }

    Ok(joint)
}
