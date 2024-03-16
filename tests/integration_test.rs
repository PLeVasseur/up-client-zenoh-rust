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
use async_std::task::{self, block_on};
use std::{sync::Arc, time};
use up_client_zenoh::UPClientZenoh;
use up_rust::ulistener::UListener;
use up_rust::{
    CallOptionsBuilder, Data, Number, RpcClient, RpcServer, UAuthority, UCode, UEntity, UMessage,
    UMessageBuilder, UMessageType, UPayload, UPayloadFormat, UResource, UResourceBuilder, UStatus,
    UTransport, UUIDBuilder, UUri,
};
use zenoh::config::Config;

fn create_utransport_uuri(index: u8) -> UUri {
    if index == 1 {
        UUri {
            entity: Some(UEntity {
                name: "entity1".to_string(),
                version_major: Some(1),
                id: Some(1111),
                ..Default::default()
            })
            .into(),
            resource: Some(UResource {
                name: "name1".to_string(),
                instance: Some("instance1".to_string()),
                message: Some("message1".to_string()),
                id: Some(1111),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        }
    } else {
        UUri {
            entity: Some(UEntity {
                name: "body.access".to_string(),
                version_major: Some(1),
                id: Some(1234),
                ..Default::default()
            })
            .into(),
            resource: Some(UResource {
                name: "door".to_string(),
                instance: Some("front_left".to_string()),
                message: Some("Door".to_string()),
                id: Some(5678),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        }
    }
}

fn create_rpcserver_uuri() -> UUri {
    UUri {
        entity: Some(UEntity {
            name: "test_rpc.app".to_string(),
            version_major: Some(1),
            id: Some(1234),
            ..Default::default()
        })
        .into(),
        resource: Some(UResourceBuilder::for_rpc_request(
            Some("SimpleTest".to_string()),
            Some(5678),
        ))
        .into(),
        ..Default::default()
    }
}

fn create_authority() -> UAuthority {
    UAuthority {
        name: Some("UAuthName".to_string()),
        number: Some(Number::Id(vec![1, 2, 3, 10, 11, 12])),
        ..Default::default()
    }
}

fn create_special_uuri() -> UUri {
    UUri {
        authority: Some(create_authority()).into(),
        ..Default::default()
    }
}

#[derive(Debug, Clone, Copy)]
struct FooListener;

impl UListener for FooListener {
    fn on_receive(&self, received: Result<UMessage, UStatus>) {
        println!("From within FooListener, received: {:?}", &received);
    }
}

#[async_std::test]
async fn test_utransport_register_and_unregister() {
    let upclient = UPClientZenoh::new(Config::default()).await.unwrap();
    let uuri = create_utransport_uuri(0);

    let foo_listener_register = FooListener;
    // Able to register
    let register_res = upclient
        .register_listener(uuri.clone(), foo_listener_register)
        .await;
    assert_eq!(register_res, Ok(()));

    let foo_listener_unregister = FooListener;
    // Able to unregister
    let unregister_res = upclient
        .unregister_listener(uuri.clone(), foo_listener_unregister)
        .await;
    assert_eq!(unregister_res, Ok(()));

    let foo_listener_unregister = FooListener;
    // Unable to unregister
    let result = upclient
        .unregister_listener(uuri.clone(), foo_listener_unregister)
        .await;
    assert_eq!(
        result,
        Err(UStatus::fail_with_code(
            UCode::NOT_FOUND,
            format!("No listeners registered for topic: {:?}", &uuri),
        ))
    );
}

// #[async_std::test]
// async fn test_rpcserver_register_and_unregister() {
//     let upclient = UPClientZenoh::new(Config::default()).await.unwrap();
//     let uuri = create_rpcserver_uuri();
//
//     // Compare the return string
//     let listener_string = upclient
//         .register_rpc_listener(uuri.clone(), Box::new(|_| {}))
//         .await
//         .unwrap();
//     assert_eq!(listener_string, "upl/0100162e04d20100_0");
//
//     // Able to ungister
//     upclient
//         .unregister_rpc_listener(uuri.clone(), &listener_string)
//         .await
//         .unwrap();
//
//     // Unable to ungister
//     let result = upclient
//         .unregister_rpc_listener(uuri.clone(), &listener_string)
//         .await;
//     assert_eq!(
//         result,
//         Err(UStatus::fail_with_code(
//             UCode::INVALID_ARGUMENT,
//             "RPC request listener doesn't exist"
//         ))
//     );
// }

#[async_std::test]
async fn test_utransport_special_uuri_register_and_unregister() {
    let upclient = UPClientZenoh::new(Config::default()).await.unwrap();
    let uuri = create_special_uuri();

    let foo_listener_register = FooListener;
    let register_res = upclient
        .register_listener(uuri.clone(), foo_listener_register)
        .await;
    assert_eq!(register_res, Ok(()));

    let foo_listener_unregister = FooListener;
    // Able to unregister
    let unregister_res = upclient
        .unregister_listener(uuri.clone(), foo_listener_unregister)
        .await;
    assert_eq!(unregister_res, Ok(()));

    let foo_listener_unregister = FooListener;
    // Unable to unregister
    let result = upclient
        .unregister_listener(uuri.clone(), foo_listener_unregister)
        .await;
    assert_eq!(
        result,
        Err(UStatus::fail_with_code(
            UCode::NOT_FOUND,
            format!("No listeners registered for topic: {:?}", &uuri),
        ))
    );
}

#[derive(Debug, Clone)]
struct PubSubTestListener {
    expected_uuri: Arc<UUri>,
    expected_data: Arc<String>,
}

impl PubSubTestListener {
    pub fn new(uuri: UUri, data: String) -> Self {
        Self {
            expected_uuri: Arc::new(uuri),
            expected_data: Arc::new(data),
        }
    }
}

impl UListener for PubSubTestListener {
    fn on_receive(&self, received: Result<UMessage, UStatus>) {
        match received {
            Ok(msg) => {
                if let Data::Value(v) = msg.payload.unwrap().data.unwrap() {
                    let value = v.into_iter().map(|c| c as char).collect::<String>();
                    assert_eq!(msg.attributes.unwrap().source.unwrap(), *self.expected_uuri);
                    assert_eq!(value, *self.expected_data);
                } else {
                    panic!("The message should be Data::Value type.");
                }
            }
            Err(ustatus) => panic!("Internal Error: {ustatus:?}"),
        }
    }
}

#[async_std::test]
async fn test_publish_and_subscribe() {
    let target_data = String::from("Hello World!");
    let upclient = UPClientZenoh::new(Config::default()).await.unwrap();
    let topic = create_utransport_uuri(0);

    println!("topic: {:?}", &topic);

    // Register the listener
    let pub_sub_test_listener = PubSubTestListener::new(topic.clone(), target_data.clone());
    let register_res = upclient
        .register_listener(topic.clone(), pub_sub_test_listener)
        .await;
    assert_eq!(register_res, Ok(()));

    let uuid_builder = UUIDBuilder::new();

    let umessage = UMessageBuilder::publish(topic.clone())
        .with_message_id(uuid_builder.build())
        .build_with_payload(
            target_data.as_bytes().to_vec().into(),
            UPayloadFormat::UPAYLOAD_FORMAT_TEXT,
        )
        .unwrap();

    println!("umessage: {:?}", umessage);

    let send_res = upclient.send(umessage).await;
    assert_eq!(send_res, Ok(()));

    // Waiting for the subscriber to receive data
    task::sleep(time::Duration::from_millis(1000)).await;

    // Cleanup
    let pub_sub_test_listener = PubSubTestListener::new(topic.clone(), target_data.clone());
    let unregister_res = upclient
        .unregister_listener(topic.clone(), pub_sub_test_listener)
        .await;
    assert_eq!(unregister_res, Ok(()));
}

#[derive(Clone)]
struct NotifTestListener {
    expected_sink: Arc<UUri>,
    expected_data: Arc<String>,
}

impl NotifTestListener {
    pub fn new(expected_sink: UUri, expected_data: String) -> Self {
        Self {
            expected_sink: Arc::new(expected_sink),
            expected_data: Arc::new(expected_data),
        }
    }
}

impl UListener for NotifTestListener {
    fn on_receive(&self, received: Result<UMessage, UStatus>) {
        match received {
            Ok(msg) => {
                if let Data::Value(v) = msg.payload.unwrap().data.unwrap() {
                    let value = v.into_iter().map(|c| c as char).collect::<String>();
                    assert_eq!(msg.attributes.unwrap().sink.unwrap(), *self.expected_sink);
                    assert_eq!(value, *self.expected_data);
                } else {
                    panic!("The message should be Data::Value type.");
                }
            }
            Err(ustatus) => panic!("Internal Error: {ustatus:?}"),
        }
    }
}

#[async_std::test]
async fn test_notification_and_subscribe() {
    let target_data = String::from("Hello World!");
    let upclient = UPClientZenoh::new(Config::default()).await.unwrap();
    let origin_uuri = create_utransport_uuri(1);
    let destination_uuri = create_utransport_uuri(2);

    // Register the listener
    let test_correct_received_listener =
        NotifTestListener::new(destination_uuri.clone(), target_data.clone());

    let register_res = upclient
        .register_listener(
            destination_uuri.clone(),
            test_correct_received_listener.clone(),
        )
        .await;
    assert_eq!(register_res, Ok(()));

    let uuid_builder = UUIDBuilder::new();

    let umessage = UMessageBuilder::notification(origin_uuri.clone(), destination_uuri.clone())
        .with_message_id(uuid_builder.build())
        .build_with_payload(
            target_data.as_bytes().to_vec().into(),
            UPayloadFormat::UPAYLOAD_FORMAT_TEXT,
        )
        .unwrap();
    let send_res = upclient.send(umessage).await;
    assert_eq!(send_res, Ok(()));

    // Waiting for the subscriber to receive data
    task::sleep(time::Duration::from_millis(1000)).await;

    // Cleanup
    let unregister_res = upclient
        .unregister_listener(
            destination_uuri.clone(),
            test_correct_received_listener.clone(),
        )
        .await;
    assert_eq!(unregister_res, Ok(()));
}

// #[async_std::test]
// async fn test_rpc_server_client() {
//     let upclient_client = UPClientZenoh::new(Config::default()).await.unwrap();
//     let upclient_server = Arc::new(UPClientZenoh::new(Config::default()).await.unwrap());
//     let request_data = String::from("This is the request data");
//     let response_data = String::from("This is the response data");
//     let uuri = create_rpcserver_uuri();
//
//     // setup RpcServer callback
//     let upclient_server_cloned = upclient_server.clone();
//     let response_data_cloned = response_data.clone();
//     let request_data_cloned = request_data.clone();
//     let callback = move |result: Result<UMessage, UStatus>| {
//         match result {
//             Ok(msg) => {
//                 let UMessage {
//                     attributes,
//                     payload,
//                     ..
//                 } = msg;
//                 // Get the UUri
//                 let source = attributes.clone().unwrap().source.unwrap();
//                 let sink = attributes.clone().unwrap().sink.unwrap();
//                 // Build the payload to send back
//                 if let Data::Value(v) = payload.unwrap().data.unwrap() {
//                     let value = v.into_iter().map(|c| c as char).collect::<String>();
//                     assert_eq!(request_data_cloned, value);
//                 } else {
//                     panic!("The message should be Data::Value type.");
//                 }
//                 let upayload = UPayload {
//                     length: Some(0),
//                     format: UPayloadFormat::UPAYLOAD_FORMAT_TEXT.into(),
//                     data: Some(Data::Value(response_data_cloned.as_bytes().to_vec())),
//                     ..Default::default()
//                 };
//                 // Set the attributes type to Response
//                 let mut uattributes = attributes.unwrap();
//                 uattributes.type_ = UMessageType::UMESSAGE_TYPE_RESPONSE.into();
//                 uattributes.sink = Some(source.clone()).into();
//                 uattributes.source = Some(sink.clone()).into();
//                 // Send back result
//                 block_on(upclient_server_cloned.send(UMessage {
//                     attributes: Some(uattributes).into(),
//                     payload: Some(upayload).into(),
//                     ..Default::default()
//                 }))
//                 .unwrap();
//             }
//             Err(ustatus) => {
//                 panic!("Internal Error: {ustatus:?}");
//             }
//         }
//     };
//     upclient_server
//         .register_rpc_listener(uuri.clone(), Box::new(callback))
//         .await
//         .unwrap();
//     // Need some time for queryable to run
//     task::sleep(time::Duration::from_millis(1000)).await;
//
//     // Run RpcClient
//     let payload = UPayload {
//         length: Some(0),
//         format: UPayloadFormat::UPAYLOAD_FORMAT_TEXT.into(),
//         data: Some(Data::Value(request_data.as_bytes().to_vec())),
//         ..Default::default()
//     };
//     let result = upclient_client
//         .invoke_method(uuri, payload, CallOptionsBuilder::default().build())
//         .await;
//
//     // Process the result
//     if let Data::Value(v) = result.unwrap().payload.unwrap().data.unwrap() {
//         let value = v.into_iter().map(|c| c as char).collect::<String>();
//         assert_eq!(response_data, value);
//     } else {
//         panic!("Failed to get result from invoke_method.");
//     }
// }
//
// #[async_std::test]
// async fn test_register_listener_with_special_uuri() {
//     let upclient1 = Arc::new(UPClientZenoh::new(Config::default()).await.unwrap());
//     let upclient1_clone = upclient1.clone();
//     let upclient2 = UPClientZenoh::new(Config::default()).await.unwrap();
//     // Create data
//     let publish_data = String::from("Hello World!");
//     let publish_data_clone = publish_data.clone();
//     let request_data = String::from("This is the request data");
//     let request_data_clone = request_data.clone();
//
//     // Register the listener
//     let listener_uuri = create_special_uuri();
//     let listener = move |result: Result<UMessage, UStatus>| match result {
//         Ok(msg) => {
//             let UMessage {
//                 attributes,
//                 payload,
//                 ..
//             } = msg;
//             let value = if let Data::Value(v) = payload.clone().unwrap().data.unwrap() {
//                 v.into_iter().map(|c| c as char).collect::<String>()
//             } else {
//                 panic!("The message should be Data::Value type.");
//             };
//             match attributes.type_.enum_value().unwrap() {
//                 UMessageType::UMESSAGE_TYPE_PUBLISH => {
//                     assert_eq!(publish_data_clone, value);
//                 }
//                 UMessageType::UMESSAGE_TYPE_REQUEST => {
//                     assert_eq!(request_data_clone, value);
//                     // Set the attributes type to Response
//                     let mut uattributes = attributes.unwrap();
//                     uattributes.type_ = UMessageType::UMESSAGE_TYPE_RESPONSE.into();
//                     // Swap source and sink
//                     (uattributes.sink, uattributes.source) =
//                         (uattributes.source.clone(), uattributes.sink.clone());
//                     // Send back result
//                     block_on(upclient1_clone.send(UMessage {
//                         attributes: Some(uattributes).into(),
//                         payload,
//                         ..Default::default()
//                     }))
//                     .unwrap();
//                 }
//                 UMessageType::UMESSAGE_TYPE_RESPONSE => {
//                     panic!("Response type");
//                 }
//                 UMessageType::UMESSAGE_TYPE_UNSPECIFIED => {
//                     panic!("Unknown type");
//                 }
//             }
//         }
//         Err(ustatus) => panic!("Internal Error: {ustatus:?}"),
//     };
//     let listener_string = upclient1
//         .register_listener(listener_uuri.clone(), Box::new(listener))
//         .await
//         .unwrap();
//
//     // send Publish
//     {
//         let mut publish_uuri = create_utransport_uuri(0);
//         publish_uuri.authority = Some(create_authority()).into();
//
//         let umessage = UMessageBuilder::publish(&publish_uuri)
//             .build_with_payload(
//                 &UUIDBuilder::new(),
//                 publish_data.as_bytes().to_vec().into(),
//                 UPayloadFormat::UPAYLOAD_FORMAT_TEXT,
//             )
//             .unwrap();
//         upclient2.send(umessage).await.unwrap();
//
//         // Waiting for the subscriber to receive data
//         task::sleep(time::Duration::from_millis(1000)).await;
//     }
//     // send Request
//     {
//         let mut request_uuri = create_rpcserver_uuri();
//         request_uuri.authority = Some(create_authority()).into();
//
//         // Run RpcClient
//         let payload = UPayload {
//             length: Some(0),
//             format: UPayloadFormat::UPAYLOAD_FORMAT_TEXT.into(),
//             data: Some(Data::Value(request_data.as_bytes().to_vec())),
//             ..Default::default()
//         };
//         let result = upclient2
//             .invoke_method(request_uuri, payload, CallOptionsBuilder::default().build())
//             .await;
//         // Process the result
//         if let Data::Value(v) = result.unwrap().payload.unwrap().data.unwrap() {
//             let value = v.into_iter().map(|c| c as char).collect::<String>();
//             assert_eq!(request_data, value);
//         } else {
//             panic!("Failed to get result from invoke_method.");
//         }
//     }
//
//     // Cleanup
//     upclient1
//         .unregister_listener(listener_uuri, &listener_string)
//         .await
//         .unwrap();
// }
