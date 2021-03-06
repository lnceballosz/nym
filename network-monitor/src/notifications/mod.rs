// Copyright 2020 - Nym Technologies SA <contact@nymtech.net>
// SPDX-License-Identifier: Apache-2.0

use super::monitor::MixnetReceiver;
use crate::monitor::NOTIFIER_DELIVERY_TIMEOUT;
use crate::notifications::test_run::TestRun;
use crate::notifications::test_timeout::TestTimeout;
use crate::run_info::{RunInfo, TestRunUpdate, TestRunUpdateReceiver};
use crypto::asymmetric::encryption::KeyPair;
use futures::StreamExt;
use log::*;
use nymsphinx::receiver::MessageReceiver;
use std::sync::Arc;
use validator_client::models::mixmining::BatchMixStatus;
use validator_client::ValidatorClientError;

mod test_run;
mod test_timeout;

#[derive(Debug)]
enum NotifierError {
    ValidatorError(ValidatorClientError),
    MalformedPacketReceived,
    NonTestPacketReceived,
}

pub(crate) struct Notifier {
    client_encryption_keypair: KeyPair,
    message_receiver: MessageReceiver,
    mixnet_receiver: MixnetReceiver,
    validator_client: Arc<validator_client::Client>,
    test_run_receiver: TestRunUpdateReceiver,
    test_run_nonce: u64,
    current_test_run: TestRun,
    test_timeout: TestTimeout,
}

impl Notifier {
    pub(crate) fn new(
        mixnet_receiver: MixnetReceiver,
        client_encryption_keypair: KeyPair,
        validator_client: Arc<validator_client::Client>,
        test_run_receiver: TestRunUpdateReceiver,
        with_detailed_report: bool,
    ) -> Notifier {
        let message_receiver = MessageReceiver::new();
        let mut current_test_run = TestRun::new(0).with_report();
        if with_detailed_report {
            current_test_run = current_test_run.with_detailed_report();
        }
        Notifier {
            client_encryption_keypair,
            message_receiver,
            mixnet_receiver,
            validator_client,
            test_run_receiver,
            test_run_nonce: 0,
            current_test_run,
            test_timeout: TestTimeout::new(),
        }
    }

    async fn on_run_start(&mut self, run_info: RunInfo) {
        self.test_run_nonce += 1;

        self.current_test_run.refresh(self.test_run_nonce);
        self.current_test_run.start_run(run_info);
    }

    async fn on_run_end(&mut self) {
        let batch_status = self.current_test_run.finish_run();
        if let Err(err) = self.notify_validator(batch_status).await {
            warn!("Failed to send batch status to validator - {:?}", err)
        }
    }

    fn on_sending_over(&mut self, nonce: u64) {
        assert_eq!(nonce, self.test_run_nonce);
        self.test_timeout.start(NOTIFIER_DELIVERY_TIMEOUT);
    }

    async fn on_test_run_update(&mut self, run_update: TestRunUpdate) {
        match run_update {
            TestRunUpdate::StartSending(run_info) => self.on_run_start(run_info).await,
            TestRunUpdate::DoneSending(nonce) => self.on_sending_over(nonce),
        }
    }

    fn on_mix_messages(&mut self, messages: Vec<Vec<u8>>) {
        for message in messages {
            if let Err(err) = self.on_message(message) {
                error!(target: "Mix receiver", "failed to process received mix packet - {:?}", err)
            }
        }
    }

    pub(crate) async fn run(&mut self) {
        debug!("Started MixnetListener");
        loop {
            tokio::select! {
                mix_messages = &mut self.mixnet_receiver.next() => {
                    self.on_mix_messages(mix_messages.expect("mix channel has failed!"));
                },
                run_update = &mut self.test_run_receiver.next() => {
                    self.on_test_run_update(run_update.expect("packet sender has died!")).await;
                }
                _ = &mut self.test_timeout => {
                    self.on_run_end().await;
                    self.test_timeout.clear();
                }
            }
        }
    }

    fn on_message(&mut self, message: Vec<u8>) -> Result<(), NotifierError> {
        let encrypted_bytes = self
            .message_receiver
            .recover_plaintext(self.client_encryption_keypair.private_key(), message)
            .map_err(|_| NotifierError::MalformedPacketReceived)?;
        let fragment = self
            .message_receiver
            .recover_fragment(&encrypted_bytes)
            .map_err(|_| NotifierError::MalformedPacketReceived)?;
        let (recovered, _) = self
            .message_receiver
            .insert_new_fragment(fragment)
            .map_err(|_| NotifierError::MalformedPacketReceived)?
            .ok_or_else(|| NotifierError::NonTestPacketReceived)?; // if it's a test packet it MUST BE reconstructed with single fragment

        let all_received = self.current_test_run.received_packet(recovered.message);
        if all_received {
            // TODO: look why this is sometimes fired even though test run is not finished
            // (and timer does not exist!)
            self.test_timeout.fire();
        }
        Ok(())
    }

    async fn notify_validator(&self, status: BatchMixStatus) -> Result<(), NotifierError> {
        self.validator_client
            .post_batch_mixmining_status(status)
            .await
            .map_err(NotifierError::ValidatorError)?;
        Ok(())
    }
}
