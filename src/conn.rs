#![allow(unused)]

use std::{hash::Hasher, sync::Arc, time::Duration};

use bluest::Characteristic;
use eyre::Context;
use futures_lite::StreamExt;
use tokio::sync::Semaphore;
use tokio_util::time::FutureExt;

use crate::{
    messages::{self, *},
    Prog, RefreshAlongsideExt,
};

const PROTOCOL_VERSION: u8 = 1;

pub struct BootloaderConnection {
    packet: Characteristic,
    control: Characteristic,
    response_chan: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,

    // semaphore used to ratelimit packets to prevent overruns
    chunk_sem: Arc<Semaphore>,
    buf: Vec<u8>,
    mtu: u16,
}

impl BootloaderConnection {
    pub async fn new(packet: Characteristic, control: Characteristic) -> eyre::Result<Self> {
        let sem = Arc::new(Semaphore::new(10));
        let (chan_send, chan_read) = tokio::sync::mpsc::unbounded_channel();

        tokio::spawn({
            let control = control.clone();
            let sem = Arc::clone(&sem);
            async move {
                let mut response_stream = match control.notify().await {
                    Ok(it) => it,
                    Err(err) => {
                        tracing::error!("Failed to get response stream: {}", err);
                        return;
                    }
                };

                while let Some(msg) = response_stream.next().await {
                    let msg = match msg {
                        Ok(msg) => msg,
                        Err(e) => {
                            tracing::error!("Response stream errored with: {}", e);
                            continue;
                        }
                    };

                    if messages::response_is_type::<CrcRequest>(&msg) {
                        // for each crc response, reset the tokens on the semaphore

                        sem.add_permits(1usize.saturating_sub(sem.available_permits()));
                    }

                    // let Some(Ok(next)) = response_stream.next().await else {
                    //     tracing::error!(
                    //         "Expecting weird duped messages but didn't get any and failed"
                    //     );
                    //     break;
                    // };

                    // if msg != next {
                    //     tracing::error!("Message not same as last: {next:?} ({msg:?})");
                    //     panic!("Shit's fucked");
                    // }

                    // tracing::trace!("Received message: {:?}", msg);

                    if chan_send.send(msg).is_err() {
                        break;
                    }
                }
            }
        });

        let mut this = Self {
            control,
            packet,
            response_chan: chan_read,
            chunk_sem: sem,
            buf: Vec::new(),
            mtu: 0,
        };

        // We must check the protocol version before doing anything else, since any other command
        // might change if the version changes.
        let proto_version = this.fetch_protocol_version().await?;
        if proto_version != PROTOCOL_VERSION {
            eyre::bail!(
                "device reports protocol version {}, we only support {}",
                proto_version,
                PROTOCOL_VERSION
            );
        }

        let mtu = this.fetch_mtu().await?;
        tracing::debug!("MTU = {} Bytes", mtu);
        this.mtu = mtu;
        Ok(this)
    }

    /// send `req` and do not fetch any response
    async fn request<R: Request>(&mut self, req: R) -> eyre::Result<()> {
        let mut buf = vec![R::OPCODE as u8];
        req.write_payload(&mut buf)?;
        tracing::trace!("--> {:?}", buf);

        // Go through an intermediate buffer to avoid writing every byte individually.
        self.buf.clear();
        self.control.write(&buf).await?;

        Ok(())
    }

    /// send `req` and expect a response.
    /// aborts if no response is received within timeout window.
    async fn request_response<R: Request>(&mut self, req: R) -> eyre::Result<R::Response> {
        self.request(req).await?;

        self.response::<R>().await
    }

    pub async fn response<R: Request>(&mut self) -> eyre::Result<R::Response> {
        loop {
            let r = self
                .response_chan
                .recv()
                // wiping memory can take a while it seems
                .timeout(Duration::from_secs(30))
                .await;

            let Ok(Some(r)) = r else {
                eyre::bail!("Response channel closed or we timed out");
            };

            tracing::trace!("<-- {:?}", r);

            if let Some(msg) = messages::parse_response::<R>(&r)? {
                return Ok(msg);
            }
        }
    }

    pub async fn fetch_protocol_version(&mut self) -> eyre::Result<u8> {
        let response = self.request_response(ProtocolVersionRequest).await;
        match response {
            Ok(version_response) => Ok(version_response.version),
            Err(e) => Err(e),
        }
    }

    pub async fn fetch_hardware_version(&mut self) -> eyre::Result<HardwareVersionResponse> {
        self.request_response(HardwareVersionRequest).await
    }

    /// Sends the `.dat` file that's zipped into our firmware DFU .zip(?)
    /// modeled after `pc-nrfutil`s `dfu_transport_serial::send_init_packet()`
    pub async fn send_init_packet(&mut self, data: &[u8]) -> eyre::Result<()> {
        tracing::debug!("Sending init packet...");
        let select_response = self.select_object_command().await?;
        tracing::debug!("Object selected: {:?}", select_response);

        let data_size = data.len() as u32;

        tracing::debug!("Creating Command...");
        self.create_command_object(data_size).await?;
        tracing::debug!("Command created");

        tracing::debug!("Streaming Data: len: {}", data_size);
        self.stream_object_data(data, None).await?;

        tracing::debug!("Done, fetching CRC");
        let received_crc = self.get_crc_upto_offset(data_size).await?.crc;
        self.check_crc(data, received_crc, 0)?;

        self.execute().await?;

        Ok(())
    }

    /// Sends the firmware image at `bin_path`.
    /// This is done in chunks to avoid exceeding our MTU  and involves periodic CRC checks.
    pub async fn send_firmware(&mut self, image: &[u8], pb: &mut Prog) -> eyre::Result<()> {
        tracing::debug!("Sending firmware image of size {}...", image.len());

        tracing::debug!("Selecting Object: type Data");
        let select_response = self.select_object_data().refresh_alongisde(pb).await?;
        tracing::debug!("Object selected: {:?}", select_response);

        let max_size = select_response.max_size;
        let mut prev_chunk_crc: u32 = 0;
        let mut offset: u32 = 0;

        for chunk in image.chunks(max_size.try_into().unwrap()) {
            pb.set_status("Erasing flash region");
            let curr_chunk_sz: u32 = chunk.len().try_into().unwrap();
            self.create_data_object(curr_chunk_sz)
                .refresh_alongisde(pb)
                .await?;

            pb.set_status("Uploading firmware");
            tracing::debug!("Streaming Data: len: {}", curr_chunk_sz);

            self.stream_object_data(chunk, Some(pb)).await?;

            offset += chunk.len() as u32;

            pb.set_status("Checking crc");

            let received_crc = self
                .get_crc_upto_offset(offset)
                .refresh_alongisde(pb)
                .await?;
            tracing::debug!("crc response: {:?}", received_crc);
            prev_chunk_crc = self.check_crc(chunk, received_crc.crc, prev_chunk_crc)?;

            pb.set_status("Executing update");
            self.execute().refresh_alongisde(pb).await?;
        }

        tracing::info!("Done.");
        Ok(())
    }

    async fn get_crc_upto_offset(&mut self, expected: u32) -> eyre::Result<CrcResponse> {
        self.request(CrcRequest).await?;
        loop {
            let crc = match self.response::<CrcRequest>().await {
                Ok(crc) => crc,
                Err(err) => {
                    tracing::debug!(
                        "Bad decode while waiting for a CRC, probably just a bodyless CRC: {}",
                        err
                    );
                    continue;
                }
            };

            if crc.offset == expected {
                return Ok(crc);
            } else if crc.offset < expected {
                tracing::trace!("Received a CRC for a prior offset (expecting: {expected}, got: {}) (probably from receipts), discarding", crc.offset);
            } else {
                eyre::bail!("Got CRC with offset in the future, not sure what happened here");
            }
        }
    }

    fn check_crc(&self, data: &[u8], received_crc: u32, initial: u32) -> eyre::Result<u32> {
        let mut digest = crc32fast::Hasher::new_with_initial(initial);
        digest.write(data);
        let expected_crc = digest.finalize();

        if expected_crc == received_crc {
            tracing::debug!("crc passed.");
            Ok(expected_crc)
        } else {
            let err_msg = eyre::format_err!(
                "crc failed: expected {} - received {}",
                expected_crc,
                received_crc
            );
            tracing::debug!("{}", err_msg);
            Err(err_msg.into())
        }
    }

    /// Sends a
    /// Request Type: `Select`
    /// Parameters:   `Object type = Command`
    pub async fn select_object_command(&mut self) -> eyre::Result<SelectResponse> {
        self.request_response(SelectRequest(ObjectType::Command))
            .await
    }

    /// Sends a
    /// Request Type: `Select`
    /// Parameters:   `Object type = Data`
    pub async fn select_object_data(&mut self) -> eyre::Result<SelectResponse> {
        self.request_response(SelectRequest(ObjectType::Data)).await
    }

    /// Sends a
    /// Request Type: `Create`
    /// Parameters:   `Object type = Command`
    ///               `size`
    pub async fn create_command_object(&mut self, size: u32) -> eyre::Result<()> {
        self.request_response(CreateObjectRequest {
            obj_type: ObjectType::Command,
            size,
        })
        .await?;
        Ok(())
    }

    /// Sends a
    /// Request Type: `Create`
    /// Parameters:   `Object type = Data`
    ///               `size`
    pub async fn create_data_object(&mut self, size: u32) -> eyre::Result<()> {
        // Note: Data objects cannot be created if no init packet has been sent. This results in an
        // `OperationNotPermitted` error.
        self.request_response(CreateObjectRequest {
            obj_type: ObjectType::Data,
            size,
        })
        .await?;
        Ok(())
    }

    pub async fn set_receipt_notification(&mut self, every_n_packets: u16) -> eyre::Result<()> {
        self.request_response(SetPrnRequest(every_n_packets))
            .await?;
        Ok(())
    }

    pub async fn fetch_mtu(&mut self) -> eyre::Result<u16> {
        Ok(self.request_response(GetMtuRequest).await?.0)
    }

    pub async fn stream_object_data(
        &mut self,
        data: &[u8],
        mut pb: Option<&mut Prog>,
    ) -> eyre::Result<()> {
        let max_chunk_size = usize::from(self.mtu);

        for chunk in data.chunks(max_chunk_size) {
            let token = self
                .chunk_sem
                .acquire()
                .timeout(Duration::from_secs(10))
                .await?
                .context("Timed out aquiring semaphore to send chunk, maybe just allow anyway?")?;

            self.packet
                .write_without_response(chunk)
                .refresh_alongside_opt(pb.as_deref_mut())
                .await?;

            if let Some(pb) = pb.as_deref_mut() {
                pb.increment(chunk.len());
            }

            // important: the semaphore is refilled by the device sending us an
            // update
            token.forget();

            // let r = self
            //     .response_chan
            //     .recv()
            //     .timeout(Duration::from_millis(100))
            //     .await;

            // tracing::debug!("Response to write: {:?}", r);
        }

        Ok(())
    }

    pub async fn get_crc(&mut self) -> eyre::Result<CrcResponse> {
        self.request_response(CrcRequest).await
    }

    // tell the target to execute whatever request setup we sent them before
    pub async fn execute(&mut self) -> eyre::Result<ExecuteResponse> {
        self.request_response(ExecuteRequest).await
    }
}
