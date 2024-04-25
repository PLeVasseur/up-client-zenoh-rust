//
// Copyright (c) 2024 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//
pub mod test_lib;

use async_std::task::{self, block_on};
use async_trait::async_trait;
use std::{
    sync::{Arc, Mutex},
    time,
};
use test_case::test_case;
use up_client_zenoh::UPClientZenoh;
use up_rust::{
    CallOptions, Data, RpcClient, UListener, UMessage, UMessageBuilder, UPayload, UPayloadFormat,
    UStatus, UTransport, UUIDBuilder, UUri,
};

struct RequestListener {
    up_client: Arc<UPClientZenoh>,
    request_data: String,
    response_data: String,
}
impl RequestListener {
    fn new(up_client: Arc<UPClientZenoh>, request_data: String, response_data: String) -> Self {
        RequestListener {
            up_client,
            request_data,
            response_data,
        }
    }
}
#[async_trait]
impl UListener for RequestListener {
    async fn on_receive(&self, msg: UMessage) {
        let UMessage {
            attributes,
            payload,
            ..
        } = msg;
        // Check the payload of request
        if let Data::Value(v) = payload.unwrap().data.unwrap() {
            let value = v.into_iter().map(|c| c as char).collect::<String>();
            assert_eq!(self.request_data, value);
        } else {
            panic!("The message should be Data::Value type.");
        }
        // Send back result
        let umessage = UMessageBuilder::response_for_request(&attributes)
            .with_message_id(UUIDBuilder::build())
            .build_with_payload(
                self.response_data.as_bytes().to_vec().into(),
                UPayloadFormat::UPAYLOAD_FORMAT_TEXT,
            )
            .unwrap();
        block_on(self.up_client.send(umessage)).unwrap();
    }
    async fn on_error(&self, err: UStatus) {
        panic!("Internal Error: {err:?}");
    }
}

struct ResponseListener {
    response_data: Arc<Mutex<String>>,
}
impl ResponseListener {
    fn new() -> Self {
        ResponseListener {
            response_data: Arc::new(Mutex::new(String::new())),
        }
    }
    fn get_response_data(&self) -> String {
        self.response_data.lock().unwrap().clone()
    }
}
#[async_trait]
impl UListener for ResponseListener {
    async fn on_receive(&self, msg: UMessage) {
        let UMessage { payload, .. } = msg;
        // Check the response data
        if let Data::Value(v) = payload.unwrap().data.unwrap() {
            let value = v.into_iter().map(|c| c as char).collect::<String>();
            *self.response_data.lock().unwrap() = value;
        } else {
            panic!("The message should be Data::Value type.");
        }
    }
    async fn on_error(&self, _err: UStatus) {
        //panic!("Internal Error: {err:?}");
    }
}

#[test_case(test_lib::create_rpcserver_uuri(Some(1), 1), test_lib::create_rpcserver_uuri(Some(1), 1); "Normal RPC UUri")]
#[test_case(test_lib::create_rpcserver_uuri(Some(1), 1), test_lib::create_special_uuri(1); "Special listen UUri")]
#[async_std::test]
async fn test_rpc_server_client(dst_uuri: UUri, listen_uuri: UUri) {
    test_lib::before_test();

    // Initialization
    let upclient_client = test_lib::create_up_client_zenoh(0, 0).await.unwrap();
    let upclient_server = Arc::new(test_lib::create_up_client_zenoh(1, 1).await.unwrap());
    let request_data = String::from("This is the request data");
    let response_data = String::from("This is the response data");

    // Setup RpcServer callback
    let request_listener = Arc::new(RequestListener::new(
        upclient_server.clone(),
        request_data.clone(),
        response_data.clone(),
    ));
    upclient_server
        .register_listener(listen_uuri.clone(), request_listener.clone())
        .await
        .unwrap();
    // Need some time for queryable to run
    task::sleep(time::Duration::from_millis(1000)).await;

    // Send Request with invoke_method
    {
        let payload = UPayload {
            format: UPayloadFormat::UPAYLOAD_FORMAT_TEXT.into(),
            data: Some(Data::Value(request_data.as_bytes().to_vec())),
            ..Default::default()
        };
        let result = upclient_client
            .invoke_method(
                dst_uuri.clone(),
                payload,
                CallOptions {
                    ttl: 1000,
                    ..Default::default()
                },
            )
            .await;

        // Process the result
        if let Data::Value(v) = result.unwrap().payload.unwrap().data.unwrap() {
            let value = v.into_iter().map(|c| c as char).collect::<String>();
            assert_eq!(response_data.clone(), value);
        } else {
            panic!("Failed to get result from invoke_method.");
        }
    }

    // Send Request with send
    {
        // Register Response callback
        let response_uuri = upclient_client.get_response_uuri();
        let response_listener = Arc::new(ResponseListener::new());
        upclient_client
            .register_listener(response_uuri.clone(), response_listener.clone())
            .await
            .unwrap();

        // Send request
        let umessage = UMessageBuilder::request(dst_uuri.clone(), response_uuri.clone(), 1000)
            .with_message_id(UUIDBuilder::build())
            .build_with_payload(
                request_data.as_bytes().to_vec().into(),
                UPayloadFormat::UPAYLOAD_FORMAT_TEXT,
            )
            .unwrap();
        upclient_client.send(umessage).await.unwrap();

        // Waiting for the callback to process data
        task::sleep(time::Duration::from_millis(5000)).await;

        // Compare the result
        assert_eq!(response_listener.get_response_data(), response_data);

        // Cleanup
        upclient_client
            .unregister_listener(response_uuri.clone(), response_listener.clone())
            .await
            .unwrap();
    }

    // Cleanup
    upclient_server
        .unregister_listener(listen_uuri.clone(), request_listener.clone())
        .await
        .unwrap();
}

struct RequestCapturingListener {
    up_client: Arc<UPClientZenoh>,
    recv_data: Arc<Mutex<String>>,
    request: Arc<Mutex<UMessage>>,
    id: String,
}
impl RequestCapturingListener {
    fn new(up_client: Arc<UPClientZenoh>, id: String) -> Self {
        Self {
            up_client,
            recv_data: Arc::new(Mutex::new(String::new())),
            request: Arc::new(Mutex::new(UMessage::new())),
            id,
        }
    }
    fn get_recv_data(&self) -> String {
        self.recv_data.lock().unwrap().clone()
    }

    fn get_request(&self) -> UMessage {
        self.request.lock().unwrap().clone()
    }
}

#[async_trait]
impl UListener for RequestCapturingListener {
    async fn on_receive(&self, msg: UMessage) {
        let mut request = self.request.lock().unwrap();
        *request = msg.clone();

        let UMessage {
            attributes,
            payload,
            ..
        } = msg;
        let umessage = UMessageBuilder::response_for_request(&attributes)
            .with_message_id(UUIDBuilder::build())
            .build_with_payload(
                self.id.as_bytes().to_vec().into(),
                UPayloadFormat::UPAYLOAD_FORMAT_TEXT,
            )
            .unwrap();
        let send_res = block_on(self.up_client.send(umessage));
        println!("{} send_res: {send_res:?}", self.id);

        // Keep a copy of the payload
        if let Data::Value(v) = payload.unwrap().data.unwrap() {
            let value = v.into_iter().map(|c| c as char).collect::<String>();
            *self.recv_data.lock().unwrap() = value;
        } else {
            panic!("The message should be Data::Value type.");
        }
    }
    async fn on_error(&self, err: UStatus) {
        panic!("Internal Error: {err:?}");
    }
}

struct ResponseCapturingListener {
    recv_data: Arc<Mutex<String>>,
    response: Arc<Mutex<UMessage>>,
}
impl ResponseCapturingListener {
    fn new() -> Self {
        Self {
            recv_data: Arc::new(Mutex::new(String::new())),
            response: Arc::new(Mutex::new(UMessage::new())),
        }
    }
    fn get_recv_data(&self) -> String {
        self.recv_data.lock().unwrap().clone()
    }

    fn get_response(&self) -> UMessage {
        self.response.lock().unwrap().clone()
    }
}

#[async_trait]
impl UListener for ResponseCapturingListener {
    async fn on_receive(&self, msg: UMessage) {
        let mut response = self.response.lock().unwrap();
        *response = msg.clone();

        println!("response from inside of response capturing listener:\n{response:#?}");

        let UMessage { payload, .. } = msg;

        // Keep a copy of the payload
        if let Data::Value(v) = payload.unwrap().data.unwrap() {
            let value = v.into_iter().map(|c| c as char).collect::<String>();
            *self.recv_data.lock().unwrap() = value;
        } else {
            panic!("The message should be Data::Value type.");
        }
    }
    async fn on_error(&self, err: UStatus) {
        panic!("Internal Error: {err:?}");
    }
}

struct RecordingListener {
    recordings: Arc<Mutex<Vec<UMessage>>>,
}

impl RecordingListener {
    pub fn new() -> Self {
        Self {
            recordings: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn retrieve_messages(&self) -> Vec<UMessage> {
        self.recordings.lock().unwrap().clone()
    }
}

#[async_trait]
impl UListener for RecordingListener {
    async fn on_receive(&self, msg: UMessage) {
        let mut recordings = self.recordings.lock().unwrap();
        recordings.push(msg);
    }

    async fn on_error(&self, err: UStatus) {
        panic!("Internal Error: {err:?}");
    }
}

#[async_std::test]
async fn test_recorder_use_case() {
    test_lib::before_test();

    // Initialization
    let upclient_client = test_lib::create_up_client_zenoh(1, 0).await.unwrap();
    let upclient_server = Arc::new(test_lib::create_up_client_zenoh(1, 1).await.unwrap());
    let upclient_recorder = Arc::new(test_lib::create_up_client_zenoh(1, 1).await.unwrap());
    let request_data = String::from("This is the request data");
    let server_id = String::from("server");

    let listen_uuri = test_lib::create_rpcserver_uuri(Some(1), 1);
    // TODO: If this works, need to make a super-special UUri which can listen to all authorities as well
    let special_uuri = test_lib::create_special_uuri(1);

    let mut transmitted_messages = Vec::new();

    let server_listener = Arc::new(RequestCapturingListener::new(
        upclient_server.clone(),
        server_id.clone(),
    ));
    upclient_server
        .register_listener(listen_uuri.clone(), server_listener.clone())
        .await
        .unwrap();

    let recorder_listener = Arc::new(RecordingListener::new());
    upclient_recorder
        .register_listener(special_uuri.clone(), recorder_listener.clone())
        .await
        .unwrap();

    // Need some time for queryable to run
    task::sleep(time::Duration::from_millis(1000)).await;

    let dst_uuri = test_lib::create_rpcserver_uuri(Some(1), 1);

    // Send Request with invoke_method
    {
        let payload = UPayload {
            format: UPayloadFormat::UPAYLOAD_FORMAT_TEXT.into(),
            data: Some(Data::Value(request_data.as_bytes().to_vec())),
            ..Default::default()
        };
        let result = upclient_client
            .invoke_method(
                dst_uuri.clone(),
                payload,
                CallOptions {
                    ttl: 1000,
                    ..Default::default()
                },
            )
            .await;

        // Process the result
        if let Data::Value(v) = result.unwrap().payload.unwrap().data.unwrap() {
            let value = v.into_iter().map(|c| c as char).collect::<String>();
            println!("response value: {value}");
            assert_eq!(server_id.clone(), value);
        } else {
            panic!("Failed to get result from invoke_method.");
        }
        let request_msg = server_listener.get_request();
        transmitted_messages.push(request_msg);
    }

    // Send Request with send
    {
        // Register Response callback
        let response_uuri = upclient_client.get_response_uuri();
        let response_listener = Arc::new(ResponseCapturingListener::new());
        upclient_client
            .register_listener(response_uuri.clone(), response_listener.clone())
            .await
            .unwrap();

        // Send request
        let request_msg = UMessageBuilder::request(dst_uuri.clone(), response_uuri.clone(), 2000)
            .with_message_id(UUIDBuilder::build())
            .build_with_payload(
                request_data.as_bytes().to_vec().into(),
                UPayloadFormat::UPAYLOAD_FORMAT_TEXT,
            )
            .unwrap();
        transmitted_messages.push(request_msg.clone());
        upclient_client.send(request_msg).await.unwrap();

        // Waiting for the callback to process data
        task::sleep(time::Duration::from_millis(5000)).await;

        // Compare the result
        assert_eq!(response_listener.get_recv_data(), server_id);

        let response_msg = response_listener.get_response();
        transmitted_messages.push(response_msg);

        // Waiting for the recording_listener
        task::sleep(time::Duration::from_millis(3000)).await;

        // Cleanup
        upclient_client
            .unregister_listener(response_uuri.clone(), response_listener.clone())
            .await
            .unwrap();
    }

    assert_eq!(request_data.clone(), server_listener.get_recv_data());
    let recorded_msgs = recorder_listener.retrieve_messages();

    println!("recorded_msgs: {recorded_msgs:#?}");
    println!("transmitted_msgs: {transmitted_messages:#?}");

    assert!(transmitted_messages
        .iter()
        .all(|item| recorded_msgs.contains(item)));
}
