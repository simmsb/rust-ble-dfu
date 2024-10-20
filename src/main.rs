mod elf;
mod init_packet;
#[macro_use]
mod macros;
mod conn;
mod messages;

use std::error::Error;
use std::future::Future;
use std::io::{stderr, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::Duration;

use bluest::btuuid::characteristics::{FIRMWARE_REVISION_STRING, MODEL_NUMBER_STRING};
use bluest::btuuid::services::DEVICE_INFORMATION;
use bluest::{Adapter, BluetoothUuidExt, Uuid};
use clap::Parser;
use color_eyre::eyre::OptionExt;
use conn::BootloaderConnection;
use elf::read_elf_image;
use futures_lite::StreamExt;
use kdam::{tqdm, BarExt as _, Column, RichProgress, Spinner};
use ondrop::OnDrop;
use tracing::metadata::LevelFilter;
use tracing::{debug, info};

#[derive(Parser)]
#[command(version, about)]
enum Cli {
    /// Dump current info
    Info,

    /// Update firmware
    Update(UpdateCmd),
}

#[derive(clap::ValueEnum, Debug, Copy, Clone)]
enum Side {
    Left,
    Right,
}

impl Side {
    fn bt_name(&self) -> &'static str {
        match self {
            Side::Left => "Glove80 LH",
            Side::Right => "Glove80 RH",
        }
    }
}

#[derive(Parser)]
struct UpdateCmd {
    side: Side,
    file: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    color_eyre::install()?;

    async_main().await
}

async fn async_main() -> Result<(), Box<dyn Error>> {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{fmt, EnvFilter};

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::WARN.into())
                .from_env_lossy()
                .add_directive("rusty_ble_test=INFO".parse().unwrap()),
        )
        .init();

    let cmd = Cli::parse();

    let adapter = Adapter::default()
        .await
        .ok_or("Bluetooth adapter not found")?;
    adapter.wait_available().await?;

    match cmd {
        Cli::Info => {
            get_info(&adapter, Side::Left.bt_name()).await?;
            get_info(&adapter, Side::Right.bt_name()).await?;
        }
        Cli::Update(UpdateCmd { side, file }) => {
            kdam::term::init(stderr().is_terminal());
            kdam::term::hide_cursor()?;

            let _show_again = OnDrop::new(|| {
                _ = kdam::term::show_cursor();
            });

            do_update(&adapter, side.bt_name(), &file).await?;
        }
    }

    Ok(())
}

struct Prog {
    inner: RichProgress,
}

impl Prog {
    fn new() -> Self {
        Self {
            inner: RichProgress::new(
                tqdm!(),
                vec![
                    Column::Spinner(Spinner::new(
                        &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
                        80.0,
                        1.0,
                    )),
                    Column::Text("[bold blue]".to_owned()),
                    Column::Animation,
                    Column::ElapsedTime,
                ],
            ),
        }
    }

    pub fn bar(&mut self) -> &mut RichProgress {
        &mut self.inner
    }

    pub fn set_status(&mut self, status: &str) {
        self.inner
            .replace(1, Column::Text(format!("[bold blue]{status}")));
        _ = self.inner.refresh();
    }

    pub fn become_tracked(&mut self, total: usize) {
        let msg = self.inner.columns.remove(1);
        self.inner.pb = tqdm!(
            total = total,
            unit_scale = true,
            unit_divisor = 1024,
            unit = "B"
        );
        self.inner.columns = vec![
            Column::Spinner(Spinner::new(
                &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
                80.0,
                1.0,
            )),
            msg,
            Column::Animation,
            Column::Percentage(1),
            Column::Text("•".to_owned()),
            Column::CountTotal,
            Column::Text("•".to_owned()),
            Column::Rate,
            Column::Text("•".to_owned()),
            Column::ElapsedTime,
        ];
        _ = self.inner.refresh();
    }

    pub fn increment(&mut self, n: usize) {
        _ = self.inner.update(n);
    }

    #[allow(unused)]
    pub fn set(&mut self, n: usize) {
        _ = self.inner.update_to(n);
    }

    pub fn refresh(&mut self) {
        _ = self.inner.refresh();
    }
}

pub(crate) trait RefreshAlongsideExt: Future + Sized {
    async fn refresh_alongisde(self, pb: &mut Prog) -> Self::Output {
        let interval = pb.bar().pb.mininterval;

        let update_fut = async {
            let mut int = tokio::time::interval(Duration::from_secs_f32(interval));
            loop {
                pb.refresh();

                int.tick().await;
            }
        };

        tokio::select! {
            v = update_fut => {
                v // actually ! (never), but the maro brokd
            }
            v = self => {
                return v;
            }
        }
    }

    async fn refresh_alongside_opt(self, pb: Option<&mut Prog>) -> Self::Output {
        if let Some(pb) = pb {
            self.refresh_alongisde(pb).await
        } else {
            self.await
        }
    }
}

impl<F: Future> RefreshAlongsideExt for F {}

async fn do_update(
    adapter: &Adapter,
    device_name: &str,
    file: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut pb = Prog::new();

    pb.set_status("Scanning");
    debug!("starting scan");
    let mut scan = adapter.scan(&[]).await?;
    debug!("scan started");
    let device = loop {
        let Some(discovered_device) = scan.next().refresh_alongisde(&mut pb).await else {
            return Ok(());
        };
        if discovered_device.adv_data.local_name.as_deref() == Some(device_name) {
            break discovered_device.device;
        }
    };

    pb.set_status("Connecting");
    adapter
        .connect_device(&device)
        .refresh_alongisde(&mut pb)
        .await?;

    pb.set_status("Getting services");

    let services = device.services().refresh_alongisde(&mut pb).await?;

    let service = services
        .iter()
        .find(|s| s.uuid() == Uuid::from_u16(0xFE59))
        .ok_or_eyre("Couldn't find the DFU service")?;

    let characteristics = service
        .discover_characteristics()
        .refresh_alongisde(&mut pb)
        .await?;

    // println!("{characteristics:#?}");

    let control_c = characteristics
        .iter()
        .find(|c| c.uuid() == Uuid::from_u128(0x8EC90001_F315_4F60_9FB8_838830DAEA50))
        .ok_or_eyre("Missing control channel service")?;
    let packet_c = characteristics
        .iter()
        .find(|c| c.uuid() == Uuid::from_u128(0x8EC90002_F315_4F60_9FB8_838830DAEA50))
        .ok_or_eyre("Missing firmware revision service")?;

    pb.set_status("Initiating DFU connection");

    let mut conn = BootloaderConnection::new(packet_c.clone(), control_c.clone())
        .refresh_alongisde(&mut pb)
        .await?;

    let elf = std::fs::read(file)?;
    let mut image = read_elf_image(&elf)?;

    conn.set_receipt_notification(8)
        .refresh_alongisde(&mut pb)
        .await?;

    let obj_select = conn
        .select_object_command()
        .refresh_alongisde(&mut pb)
        .await;
    tracing::debug!("select object response: {:?}", obj_select);

    let version = conn
        .fetch_protocol_version()
        .refresh_alongisde(&mut pb)
        .await?;
    tracing::debug!("protocol version: {}", version);

    let hw_version = conn
        .fetch_hardware_version()
        .refresh_alongisde(&mut pb)
        .await?;
    tracing::debug!("hardware version: {:?}", hw_version);

    while image.len() % 4 != 0 {
        image.push(0xff);
    }

    pb.set_status("Initiating DFU update");

    let init_packet = init_packet::build_init_packet(&image);
    conn.send_init_packet(&init_packet)
        .refresh_alongisde(&mut pb)
        .await?;

    pb.become_tracked(image.len());

    pb.set_status("Uploading firmware");

    conn.send_firmware(&image, &mut pb).await?;

    pb.set_status("Done, device rebooting");

    Ok(())
}

async fn get_info(adapter: &Adapter, device_name: &str) -> Result<(), Box<dyn Error>> {
    info!("starting scan");
    let mut scan = adapter.scan(&[]).await?;
    info!("scan started");
    let device = loop {
        let Some(discovered_device) = scan.next().await else {
            return Ok(());
        };
        if discovered_device.adv_data.local_name.as_deref() == Some(device_name) {
            break discovered_device.device;
        }
    };

    adapter.connect_device(&device).await?;
    let services = device.discover_services().await?;

    let device_info_service = services
        .iter()
        .find(|s| s.uuid() == DEVICE_INFORMATION)
        .ok_or_eyre("Couldn't find the Device info service")?;

    let characteristics = device_info_service.discover_characteristics().await?;
    let model_number_c = characteristics
        .iter()
        .find(|c| c.uuid() == MODEL_NUMBER_STRING)
        .ok_or_eyre("Missing model number service")?;
    let firmware_revision_c = characteristics
        .iter()
        .find(|c| c.uuid() == FIRMWARE_REVISION_STRING)
        .ok_or_eyre("Missing firmware revision service")?;
    let model_number = String::from_utf8(model_number_c.read().await?)?;
    let firmware_revision = String::from_utf8(firmware_revision_c.read().await?)?;
    println!("({device_name}) device info: {model_number}: {firmware_revision}");

    Ok(())
}
