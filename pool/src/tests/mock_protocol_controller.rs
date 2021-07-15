// Copyright (c) 2021 MASSA LABS <info@massa.net>

use std::collections::HashMap;

use communication::protocol::{
    ProtocolCommand, ProtocolCommandSender, ProtocolPoolEvent, ProtocolPoolEventReceiver,
};
use models::{Operation, OperationId};
use time::UTime;
use tokio::{sync::mpsc, time::sleep};

const CHANNEL_SIZE: usize = 256;

pub struct MockProtocolController {
    protocol_command_rx: mpsc::Receiver<ProtocolCommand>,
    pool_event_tx: mpsc::Sender<ProtocolPoolEvent>,
}

impl MockProtocolController {
    pub fn new() -> (Self, ProtocolCommandSender, ProtocolPoolEventReceiver) {
        let (protocol_command_tx, protocol_command_rx) =
            mpsc::channel::<ProtocolCommand>(CHANNEL_SIZE);
        let (pool_event_tx, pool_event_rx) = mpsc::channel::<ProtocolPoolEvent>(CHANNEL_SIZE);
        (
            MockProtocolController {
                protocol_command_rx,
                pool_event_tx,
            },
            ProtocolCommandSender(protocol_command_tx),
            ProtocolPoolEventReceiver(pool_event_rx),
        )
    }

    pub async fn wait_command<F, T>(&mut self, timeout: UTime, filter_map: F) -> Option<T>
    where
        F: Fn(ProtocolCommand) -> Option<T>,
    {
        let timer = sleep(timeout.into());
        tokio::pin!(timer);
        loop {
            tokio::select! {
                cmd_opt = self.protocol_command_rx.recv() => match cmd_opt {
                    Some(orig_cmd) => if let Some(res_cmd) = filter_map(orig_cmd) { return Some(res_cmd); },
                    None => panic!("Unexpected closure of protocol command channel."),
                },
                _ = &mut timer => return None
            }
        }
    }

    pub async fn received_operations(&mut self, ops: HashMap<OperationId, Operation>) {
        self.pool_event_tx
            .send(ProtocolPoolEvent::ReceivedOperations(ops))
            .await
            .expect("could not send protocol pool event");
    }

    // ignore all commands while waiting for a future
    pub async fn ignore_commands_while<FutureT: futures::Future + Unpin>(
        &mut self,
        mut future: FutureT,
    ) -> FutureT::Output {
        loop {
            tokio::select!(
                res = &mut future => return res,
                cmd = self.protocol_command_rx.recv() => match cmd {
                    Some(_) => {},
                    None => return future.await,  // if the protocol controlled dies, wait for the future to finish
                }
            );
        }
    }
}
