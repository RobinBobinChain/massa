use std::thread::JoinHandle;

use crossbeam::channel::{unbounded, Receiver, Sender};
use massa_serialization::{DeserializeError, Deserializer};
use peernet::{network_manager::SharedActiveConnections, peer_id::PeerId};

use self::messages::{
    OperationMessageDeserializer, OperationMessageDeserializerArgs, OperationMessageSerializer,
};

mod messages;

pub struct OperationHandler {
    pub operation_retrieval_thread: Option<JoinHandle<()>>,
    pub operation_propagation_thread: Option<JoinHandle<()>>,
}

impl OperationHandler {
    pub fn new(
        active_connections: SharedActiveConnections,
        receiver: Receiver<(PeerId, Vec<u8>)>,
    ) -> Self {
        //TODO: Define real data
        let (_internal_sender, internal_receiver): (Sender<()>, Receiver<()>) = unbounded();
        let operation_retrieval_thread = std::thread::spawn(move || {
            //TODO: Real values
            let operation_message_deserializer =
                OperationMessageDeserializer::new(OperationMessageDeserializerArgs {
                    max_datastore_value_length: 10000,
                    max_function_name_length: 10000,
                    max_op_datastore_entry_count: 10000,
                    max_op_datastore_key_length: 100,
                    max_op_datastore_value_length: 10000,
                    max_operations: 10000,
                    max_operations_prefix_ids: 10000,
                    max_parameters_size: 10000,
                });
            //TODO: Real logic
            loop {
                match receiver.recv() {
                    Ok((peer_id, message)) => {
                        let (rest, message) = operation_message_deserializer
                            .deserialize::<DeserializeError>(&message)
                            .unwrap();
                        if !rest.is_empty() {
                            println!("Error: message not fully consumed");
                            return;
                        }
                        println!("Received message from {:?}: {:?}", peer_id, message);
                    }
                    Err(err) => {
                        println!("Error: {:?}", err);
                        return;
                    }
                }
            }
        });

        let operation_propagation_thread = std::thread::spawn({
            let _active_connections = active_connections.clone();
            move || {
                let _operation_message_serializer = OperationMessageSerializer::new();
                //TODO: Real logic
                loop {
                    match internal_receiver.recv() {
                        Ok(_data) => {
                            // Example to send data
                            // {
                            //     let active_connections = active_connections.read();
                            //     for (peer_id, connection) in active_connections.iter() {
                            //         println!("Sending message to {:?}", peer_id);
                            //         let buf = Vec::new();
                            //         operation_message_serializer.serialize(&data, &mut buf).unwrap();
                            //         connection.send_message(&buf);
                            //     }
                            // }
                            println!("Received message");
                        }
                        Err(err) => {
                            println!("Error: {:?}", err);
                            return;
                        }
                    }
                }
            }
        });
        Self {
            operation_retrieval_thread: Some(operation_retrieval_thread),
            operation_propagation_thread: Some(operation_propagation_thread),
        }
    }

    pub fn stop(&mut self) {
        if let Some(thread) = self.operation_retrieval_thread.take() {
            thread.join().unwrap();
        }
        if let Some(thread) = self.operation_propagation_thread.take() {
            thread.join().unwrap();
        }
    }
}
