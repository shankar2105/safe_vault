// Copyright 2015 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement, version 1.0.  This, along with the
// Licenses can be found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

use std::collections::HashMap;

use sodiumoxide::crypto::sign::PublicKey;

use chunk_store::ChunkStore;
use default_chunk_store;
use error::{ClientError, InternalError};
use maidsafe_utilities::serialisation::{deserialise, serialise};
use mpid_messaging::{self, MAX_INBOX_SIZE, MAX_OUTBOX_SIZE, MpidMessageWrapper, MpidMessage};
use routing::{Authority, Data, PlainData, RequestContent, RequestMessage};
use vault::RoutingNode;
use xor_name::XorName;

#[derive(RustcEncodable, RustcDecodable, PartialEq, Eq, Debug, Clone)]
struct MailBox {
    allowance: u64,
    used_space: u64,
    space_available: u64,
    // key: msg or header's name; value: sender's public key
    mail_box: HashMap<XorName, Option<PublicKey>>,
}

impl MailBox {
    fn new(allowance: u64) -> MailBox {
        MailBox {
            allowance: allowance,
            used_space: 0,
            space_available: allowance,
            mail_box: HashMap::new()
        }
    }


    fn put(&mut self, size: u64, entry: &XorName, public_key: &Option<PublicKey>) -> bool {
        if size > self.space_available {
            return false;
        }
        if self.mail_box.contains_key(entry) {
            return false;
        }
        match self.mail_box.insert(entry.clone(), public_key.clone()) {
            Some(_) => {
                self.used_space += size;
                self.space_available -= size;
                true
            }
            None => false,
        }
    }

    #[allow(dead_code)]
    fn remove(&mut self, size: u64, entry: &XorName) -> bool {
        if !self.mail_box.contains_key(entry) {
            return false;
        }
        self.used_space -= size;
        self.space_available += size;
        match self.mail_box.remove(entry) {
            Some(_) => {
                self.used_space -= size;
                self.space_available += size;
                true
            }
            None => false,
        }
    }

    fn has(&self, entry: &XorName) -> bool {
        self.mail_box.contains_key(entry)
    }

    fn names(&self) -> Vec<XorName> {
        use itertools::Itertools;
        self.mail_box.iter().map(|pair| pair.0.clone()).collect_vec()
    }
}

#[derive(RustcEncodable, RustcDecodable, PartialEq, Eq, Debug, Clone)]
struct Account {
    // account owners' registered client proxies
    clients: Vec<Authority>,
    inbox: MailBox,
    outbox: MailBox,
}

impl Default for Account {
    // FIXME: Account Creation process required
    //   To bypass the the process for a simple network, allowance is granted by default
    fn default() -> Account {
        Account {
            clients: Vec::new(),
            inbox: MailBox::new(MAX_INBOX_SIZE as u64),
            outbox: MailBox::new(MAX_OUTBOX_SIZE as u64),
        }
    }
}

impl Account {
    fn put_into_outbox(&mut self, size: u64, entry: &XorName,
                       public_key: &Option<PublicKey>) -> bool {
        self.outbox.put(size, entry, public_key)
    }

    fn put_into_inbox(&mut self, size: u64, entry: &XorName,
                      public_key: &Option<PublicKey>) -> bool {
        self.inbox.put(size, entry, public_key)
    }

    #[allow(dead_code)]
    fn remove_from_outbox(&mut self, size: u64, entry: &XorName) -> bool {
        self.outbox.remove(size, entry)
    }

    #[allow(dead_code)]
    fn remove_from_inbox(&mut self, size: u64, entry: &XorName) -> bool {
        self.inbox.remove(size, entry)
    }

    fn has_in_outbox(&self, entry: &XorName) -> bool {
        self.outbox.has(entry)
    }

    fn register_online(&mut self, client: &Authority) {
        match client.clone() {
            Authority::Client { .. } => {
                if self.clients.contains(&client) {
                    warn!("client {:?} already registered", client)
                } else {
                    self.clients.push(client.clone());
                }
            }
            _ => warn!("trying to register non-client {:?} as client", client),
        }
    }

    fn received_headers(&self) -> Vec<XorName> {
        self.inbox.names()
    }

    fn registered_clients(&self) -> &Vec<Authority> {
        &self.clients
    }
}

pub struct MpidManager {
    accounts: HashMap<XorName, Account>,
    chunk_store_inbox: ChunkStore,
    chunk_store_outbox: ChunkStore,
}

impl MpidManager {
    pub fn new() -> MpidManager {
        MpidManager {
            accounts: HashMap::new(),
            chunk_store_inbox: default_chunk_store::new().unwrap(),
            chunk_store_outbox: default_chunk_store::new().unwrap(),
        }
    }

    // The name of the PlainData is expected to be the mpidheader or mpidmessage name
    // The content of the PlainData is execpted to be the serialised MpidMessageWrapper
    // holding mpidheader or mpidmessage
    pub fn handle_put(&mut self, routing_node: &RoutingNode, request: &RequestMessage)
            -> Result<(), InternalError> {
        let (data, message_id) = match request.content {
            RequestContent::Put(Data::PlainData(ref data), ref message_id) => {
                (data.clone(), message_id.clone())
            }
            _ => unreachable!("Error in vault demuxing"),
        };
        let mpid_message_wrapper = unwrap_option!(deserialise_wrapper(data.value()),
                                                  "Failed to parse MpidMessageWrapper");
        match mpid_message_wrapper {
            MpidMessageWrapper::PutHeader(_mpid_header) => {
                if self.chunk_store_inbox.has_chunk(&data.name()) {
                    return Err(InternalError::Client(ClientError::DataExists));;
                }
                // TODO: how the sender's public key get retained?
                if self.accounts
                       .entry(request.dst.get_name().clone())
                       .or_insert(Account::default())
                       .put_into_inbox(data.payload_size() as u64, &data.name(), &None) {
                    let _ = self.chunk_store_inbox.put(&data.name(), data.value());
                }
            }
            MpidMessageWrapper::PutMessage(mpid_message) => {
                if self.chunk_store_outbox.has_chunk(&data.name()) {
                    return Err(InternalError::Client(ClientError::DataExists));
                }
                // TODO: how the sender's public key get retained?
                if self.accounts
                       .entry(mpid_message.header().sender_name().clone())
                       .or_insert(Account::default())
                       .put_into_outbox(data.payload_size() as u64, &data.name(), &None) {
                    match self.chunk_store_outbox.put(&data.name(), data.value()) {
                        Err(err) => {
                            error!("Failed to store the full message to disk: {:?}", err);
                            return Err(InternalError::ChunkStore(err));
                        }
                        _ => {}
                    }
                    // Send notification to receiver's MpidManager
                    let src = request.dst.clone();
                    let dst = Authority::ClientManager(mpid_message.recipient().clone());
                    let wrapper = MpidMessageWrapper::PutHeader(mpid_message.header().clone());

                    let serialised_wrapper = match serialise(&wrapper) {
                        Ok(encoded) => encoded,
                        Err(error) => {
                            error!("Failed to serialise PutHeader wrapper: {:?}", error);
                            return Err(InternalError::Serialisation(error));
                        }
                    };
                    let name = match mpid_messaging::mpid_header_name(mpid_message.header()) {
                        Some(name) => name,
                        None => {
                            error!("Failed to calculate name of the header");
                            return Err(InternalError::Client(ClientError::NoSuchAccount));
                        }
                    };
                    let notification = Data::PlainData(PlainData::new(name, serialised_wrapper));
                    let _ = routing_node.send_put_request(src, dst, notification, message_id.clone());
                }
            }
            _ => unreachable!("Error in vault demuxing"),
        }
        Ok(())
    }

    pub fn handle_post(&mut self, routing_node: &RoutingNode, request: &RequestMessage)
            -> Result<(), InternalError> {
        let (data, message_id) = match request.content {
            RequestContent::Post(Data::PlainData(ref data), ref message_id) => {
                (data.clone(), message_id.clone())
            }
            _ => unreachable!("Error in vault demuxing"),
        };
        let mpid_message_wrapper = unwrap_option!(deserialise_wrapper(data.value()),
                                                  "Failed to parse MpidMessageWrapper");
        match mpid_message_wrapper {
            MpidMessageWrapper::Online => {
                let account = self.accounts
                    .entry(request.dst.get_name().clone())
                    .or_insert(Account::default());
                account.register_online(&request.src);
                // For each received header in the inbox, fetch the full message from the sender
                let received_headers = account.received_headers();
                for header in received_headers.iter() {
                    match self.chunk_store_inbox.get(&header) {
                        Ok(serialised_wrapper) => {
                            let wrapper = unwrap_option!(deserialise_wrapper(&serialised_wrapper[..]),
                                                         "Failed to parse MpidMessageWrapper");
                            match wrapper {
                                MpidMessageWrapper::PutHeader(mpid_header) => {
                                    // fetch full message from the sender
                                    let target = Authority::ClientManager(mpid_header.sender_name().clone());
                                    let request_wrapper = MpidMessageWrapper::GetMessage(mpid_header.clone());
                                    let serialised_request = match serialise(&request_wrapper) {
                                        Ok(encoded) => encoded,
                                        Err(error) => {
                                            error!("Failed to serialise GetMessage wrapper: {:?}", error);
                                            continue;
                                        }
                                    };
                                    let name = match mpid_messaging::mpid_header_name(&mpid_header) {
                                        Some(name) => name,
                                        None => {
                                            error!("Failed to calculate name of the header");
                                            continue;
                                        }
                                    };
                                    let data = Data::PlainData(PlainData::new(name, serialised_request));
                                    let _ = routing_node.send_post_request(request.dst.clone(),
                                        target, data, message_id.clone());
                                }
                                _ => {}
                            }
                        }
                        Err(_) => {}
                    }
                }
            }
            MpidMessageWrapper::GetMessage(mpid_header) => {
                let header_name = match mpid_messaging::mpid_header_name(&mpid_header) {
                    Some(name) => name,
                    None => {
                        error!("Failed to calculate name of the header");
                        let _ = routing_node.send_post_failure(request.dst.clone(),
                            request.src.clone(), request.clone(), Vec::new(), message_id);
                        return Ok(());
                    }
                };
                match self.chunk_store_outbox.get(&header_name) {
                    Ok(serialised_wrapper) => {
                        let wrapper = unwrap_option!(deserialise_wrapper(&serialised_wrapper[..]),
                                                     "Failed to parse MpidMessageWrapper");
                        match wrapper {
                            MpidMessageWrapper::PutMessage(mpid_message) => {
                                let message_name = match mpid_messaging::mpid_message_name(&mpid_message) {
                                    Some(name) => name,
                                    None => {
                                        error!("Failed to calculate name of the message");
                                        let _ = routing_node.send_post_failure(request.dst.clone(),
                                            request.src.clone(), request.clone(), Vec::new(), message_id);
                                        return Ok(());
                                    }
                                };
                                if (message_name == header_name) &&
                                   (mpid_message.recipient() == request.src.get_name()) {
                                    let data = Data::PlainData(PlainData::new(message_name, serialised_wrapper));
                                    let _ = routing_node.send_post_request(request.dst.clone(),
                                        request.src.clone(), data, message_id.clone());
                                }
                            }
                            _ => {
                                let _ = routing_node.send_post_failure(request.dst.clone(),
                                    request.src.clone(), request.clone(), Vec::new(), message_id);
                            }
                        }
                    }
                    Err(_) => {
                        let _ = routing_node.send_post_failure(request.dst.clone(),
                            request.src.clone(), request.clone(), Vec::new(), message_id);
                    }
                }
            }
            MpidMessageWrapper::PutMessage(mpid_message) => {
                match self.accounts.get(request.dst.get_name()) {
                    Some(receiver) => {
                        let clients = receiver.registered_clients();
                        for client in clients.iter() {
                            if mpid_message.recipient() == request.dst.get_name() {
                                let _ = routing_node.send_post_request(request.dst.clone(),
                                    client.clone(), Data::PlainData(data.clone()), message_id.clone());
                            }
                        }
                    }
                    None => warn!("can not find the account {:?}", request.dst.get_name().clone()),
                }
            }
            MpidMessageWrapper::OutboxHas(header_names) => {
                if let Some(ref account) = self.accounts.get(&request.dst.get_name().clone()) {
                    if account.registered_clients().iter()
                                                   .any(|authority| *authority == request.src) {
                        let names_in_outbox = header_names.iter()
                                                          .filter(|name| account.has_in_outbox(name))
                                                          .cloned()
                                                          .collect::<Vec<XorName>>();
                        let mut mpid_headers = vec![];

                        for name in names_in_outbox.iter() {
                            if let Ok(data) = self.chunk_store_outbox.get(name) {
                                let mpid_message: MpidMessage = unwrap_result!(deserialise(&data));
                                mpid_headers.push(mpid_message.header().clone());
                            }
                        }

                        let src = request.dst.clone();
                        let dst = request.src.clone();
                        let wrapper = MpidMessageWrapper::OutboxHasResponse(mpid_headers);
                        let serialised_wrapper = match serialise(&wrapper) {
                            Ok(serialised) => serialised,
                            Err(error) => {
                                error!("Failed to serialise OutboxHasResponse wrapper: {:?}", error);
                                return Err(InternalError::Serialisation(error));
                            }
                        };
                        let data = Data::PlainData(PlainData::new(request.dst.get_name().clone(), serialised_wrapper));
                        try!(routing_node.send_post_request(src, dst, data, message_id.clone()));
                    }
                }
            }
            MpidMessageWrapper::GetOutboxHeaders => {
                if let Some(ref account) = self.accounts.get(&request.dst.get_name().clone()) {
                    if account.registered_clients().iter()
                                                   .any(|authority| *authority == request.src) {
                        let mut mpid_headers = vec![];

                        for name in account.received_headers().iter() {
                            if let Ok(data) = self.chunk_store_outbox.get(name) {
                                let mpid_message: MpidMessage = unwrap_result!(deserialise(&data));
                                mpid_headers.push(mpid_message.header().clone());
                            }
                        }

                        let src = request.dst.clone();
                        let dst = request.src.clone();
                        let wrapper = MpidMessageWrapper::GetOutboxHeadersResponse(mpid_headers);
                        let serialised_wrapper = match serialise(&wrapper) {
                            Ok(serialised) => serialised,
                            Err(error) => {
                                error!("Failed to serialise OutboxHasResponse wrapper: {:?}", error);
                                return Err(InternalError::Serialisation(error));
                            }
                        };
                        let data = Data::PlainData(PlainData::new(request.dst.get_name().clone(), serialised_wrapper));
                        try!(routing_node.send_post_request(src, dst, data, message_id.clone()));
                    }
                }
            }
            _ => unreachable!("Error in vault demuxing"),
        }
        Ok(())
    }
}

fn deserialise_wrapper(serialised_wrapper: &[u8]) -> Option<MpidMessageWrapper> {
    match deserialise::<MpidMessageWrapper>(serialised_wrapper) {
        Ok(data) => Some(data),
        Err(_) => None
    }
}