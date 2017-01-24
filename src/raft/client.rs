use raft::{
    SyncServiceClient, ClientClusterInfo, RaftMsg,
    RaftStateMachine, LogEntry,
    ClientQryResponse, ClientCmdResponse};
use raft::state_machine::OpType;
use raft::state_machine::master::{ExecResult, ExecError};
use raft::state_machine::callback::client::SUBSCRIPTIONS;
use bincode::{SizeLimit, serde as bincode};
use std::collections::{HashMap, BTreeMap, HashSet};
use std::iter::FromIterator;
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::sync::atomic::{AtomicU64, Ordering};
use std::cell::RefCell;
use std::sync::Arc;
use bifrost_hasher::{hash_str, hash_bytes};
use rand;
use rpc;

const ORDERING: Ordering = Ordering::Relaxed;

#[derive(Debug)]
pub enum ClientError {
    LeaderIdValid,
    ServerUnreachable,
}

struct QryMeta {
    pos: AtomicU64
}

struct Members {
    clients: BTreeMap<u64, Arc<SyncServiceClient>>,
    id_map: HashMap<u64, String>,
}

pub struct RaftClient {
    qry_meta: QryMeta,
    members: RwLock<Members>,
    leader_id: AtomicU64,
    last_log_id: AtomicU64,
    last_log_term: AtomicU64,
    service_id: u64,
}

impl RaftClient {
    pub fn new(servers: Vec<String>, service_id: u64) -> Result<Arc<RaftClient>, ClientError> {
        let mut client = RaftClient {
            qry_meta: QryMeta {
                pos: AtomicU64::new(rand::random::<u64>())
            },
            members: RwLock::new(Members {
                clients: BTreeMap::new(),
                id_map: HashMap::new()
            }),
            leader_id: AtomicU64::new(0),
            last_log_id: AtomicU64::new(0),
            last_log_term: AtomicU64::new(0),
            service_id: service_id,
        };
        let init = {
            let mut members = client.members.write().unwrap();
            client.update_info(
                &mut members,
                &HashSet::from_iter(servers)
            )
        };
        match init {
            Ok(_) => Ok(Arc::new(client)),
            Err(e) => Err(e)
        }
    }

   fn update_info(&self, members: &mut RwLockWriteGuard<Members>, addrs: &HashSet<String>) -> Result<(), ClientError> {
        let info: ClientClusterInfo;
        let mut cluster_info = None;
        for server_addr in addrs {
            let id = hash_str(server_addr.clone());
            if !members.clients.contains_key(&id) {
                match rpc::DEFAULT_CLIENT_POOL.get(&server_addr) {
                    Ok(client) => {
                        members.clients.insert(id, SyncServiceClient::new(self.service_id, client));
                    },
                    Err(_) => {continue;}
                }
            }
            let mut client = members.clients.get(&id).unwrap();
            if let Ok(Ok(info)) = client.c_server_cluster_info() {
                if info.leader_id != 0 {
                    cluster_info = Some(info);
                    break;
                }
            }
        }
        match cluster_info {
            Some(info) => {
                let remote_members = info.members;
                let mut remote_ids = HashSet::with_capacity(remote_members.len());
                members.id_map.clear();
                for (id, addr) in remote_members {
                    members.id_map.insert(id, addr);
                    remote_ids.insert(id);
                }
                let mut connected_ids = HashSet::with_capacity(members.clients.len());
                for id in members.clients.keys() {connected_ids.insert(*id);}
                let ids_to_remove = connected_ids.difference(&remote_ids);
                for id in ids_to_remove {members.clients.remove(id);}
                for id in remote_ids.difference(&connected_ids) {
                    let addr = members.id_map.get(id).unwrap().clone();
                    if !members.clients.contains_key(id) {
                        if let Ok(client) = rpc::DEFAULT_CLIENT_POOL.get(&addr) {
                            members.clients.insert(*id, SyncServiceClient::new(self.service_id, client));
                        }
                    }
                }
                self.leader_id.store(info.leader_id, ORDERING);
                Ok(())
            },
            None => Err(ClientError::ServerUnreachable),
        }
    }

    pub fn execute<R>(&self, sm_id: u64, msg: &RaftMsg<R>) -> Result<R, ExecError> {
        let (fn_id, op, req_data) = msg.encode();
        let response = match op {
            OpType::QUERY => {
                self.query(sm_id, fn_id, &req_data, 0)
            },
            OpType::COMMAND | OpType::SUBSCRIBE => {
                self.command(sm_id, fn_id, &req_data, 0)
            },
        };
        match response {
            Ok(data) => {
                match data {
                    Ok(data) => Ok(msg.decode_return(&data)),
                    Err(e) => Err(e)
                }
            },
            Err(e) => Err(e)
        }
    }

    pub fn subscribe
    <M, R, F>
    (&self, sm_id: u64, msg: M, f: F) -> Result<(), ExecError>
    where M: RaftMsg<R> + 'static,
          F: FnOnce(R) + 'static + Send + Sync
    {
        let service_id = self.service_id;
        let (fn_id, _, pattern_data) = msg.encode();
        let wrapper_fn = move |data: Vec<u8>| {
            f(msg.decode_return(&data))
        };
        let pattern_id = hash_bytes(&pattern_data.as_slice());
        let key = (service_id, service_id, pattern_id);
        let mut subs_map = SUBSCRIPTIONS.write().unwrap();
        let mut subs_lst = subs_map.entry(key).or_insert_with(|| Vec::new());
        subs_lst.push(Box::new(wrapper_fn));
        Ok(())
    }

    pub fn current_leader_id(&self) -> u64 {self.leader_id.load(ORDERING)}

    fn query(&self, sm_id: u64, fn_id: u64, data: &Vec<u8>, depth: usize) -> Result<ExecResult, ExecError> {
        let pos = self.qry_meta.pos.fetch_add(1, ORDERING);
        let mut num_members = 0;
        let res = {
            let members = self.members.read().unwrap();
            let mut client = {
                let members_count = members.clients.len();
                members.clients.values().nth(pos as usize % members_count).unwrap()
            };
            num_members = members.clients.len();
            client.c_query(self.gen_log_entry(sm_id, fn_id, data))
        };
        match res {
            Ok(Ok(res)) => {
                match res {
                    ClientQryResponse::LeftBehind => {
                        if depth >= num_members {
                            Err(ExecError::TooManyRetry)
                        } else {
                            self.query(sm_id, fn_id, data, depth + 1)
                        }
                    },
                    ClientQryResponse::Success{
                        data: data,
                        last_log_term: last_log_term,
                        last_log_id: last_log_id
                    } => {
                        swap_when_greater(&self.last_log_id, last_log_id);
                        swap_when_greater(&self.last_log_term, last_log_term);
                        Ok(data)
                    },
                }
            },
            _ => Err(ExecError::Unknown)
        }
    }

    fn command(&self, sm_id: u64, fn_id: u64, data: &Vec<u8>, depth: usize) -> Result<ExecResult, ExecError> {
        enum FailureAction {
            SwitchLeader,
            UpdateInfo,
            NotLeader,
            NotCommitted,
        }
        let failure = {
            let members = self.members.read().unwrap();
            let num_members = members.clients.len();
            if depth >= num_members {
                return Err(ExecError::TooManyRetry)
            };
            let mut leader = {
                let leader_id = self.leader_id.load(ORDERING);
                if members.clients.contains_key(&leader_id) {
                    Some(leader_id)
                } else {
                    None
                }
            };
            match leader {
                Some(leader_id) => {
                    let client = members.clients.get(&leader_id).unwrap();
                    match client.c_command(self.gen_log_entry(sm_id, fn_id, data)) {
                        Ok(Ok(ClientCmdResponse::Success{
                                    data: data, last_log_term: last_log_term,
                                    last_log_id: last_log_id
                                })) => {
                            swap_when_greater(&self.last_log_id, last_log_id);
                            swap_when_greater(&self.last_log_term, last_log_term);
                            return Ok(data);
                        },
                        Ok(Ok(ClientCmdResponse::NotLeader(leader_id))) => {
                            self.leader_id.store(leader_id, ORDERING);
                            FailureAction::NotLeader
                        },
                        Ok(Ok(ClientCmdResponse::NotCommitted)) => {
                            FailureAction::NotCommitted
                        },
                        Err(e) => {
                            println!("CLIENT: E1 - {} - {:?}", leader_id, e);
                            FailureAction::SwitchLeader // need switch server for leader
                        }
                        Ok(Err(e)) => {
                            println!("CLIENT: E2 - {} - {:?}", leader_id, e);
                            FailureAction::SwitchLeader // need switch server for leader
                        }
                    }
                },
                None => FailureAction::UpdateInfo // need update members
            }
        }; //
        match failure {
            FailureAction::UpdateInfo => {
                let mut members = self.members.write().unwrap();
                let mut members_addrs = HashSet::new();
                for address in members.id_map.values() {
                    members_addrs.insert(address.clone());

                }
                self.update_info(&mut members, &members_addrs);
                println!("CLIENT: Updating info");
            },
            FailureAction::SwitchLeader => {
                let members = self.members.read().unwrap();
                let num_members = members.clients.len();
                let pos = self.qry_meta.pos.load(ORDERING);
                let leader_id = self.leader_id.load(ORDERING);
                let index = members.clients.keys()
                    .nth(pos as usize % num_members)
                    .unwrap();
                self.leader_id.compare_and_swap(leader_id, *index, ORDERING);
                println!("CLIENT: Switch leader");
            },
            FailureAction::NotCommitted => {
                return Err(ExecError::NotCommitted)
            },
            _ => {}
        }
        self.command(sm_id, fn_id, data, depth + 1)
    }

    fn gen_log_entry(&self, sm_id: u64, fn_id: u64, data: &Vec<u8>) -> LogEntry {
        LogEntry {
            id: self.last_log_id.load(ORDERING),
            term: self.last_log_term.load(ORDERING),
            sm_id: sm_id,
            fn_id: fn_id,
            data: data.clone()
        }
    }
}

fn swap_when_greater(atomic: &AtomicU64, value: u64) {
    let mut orig_num = atomic.load(ORDERING);
    loop {
        if orig_num >= value {
            return;
        }
        let actual = atomic.compare_and_swap(orig_num, value, ORDERING);
        if actual == orig_num {
            return;
        } else {
            orig_num = actual;
        }
    }
}