use std::time::Duration;

use anyhow::Result;

use autd3::prelude::*;
use autd3_link_ethercrab::*;

const INTERVAL: Duration = Duration::from_millis(5);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let mut autd = Controller::builder([AUTD3::new(Vector3::zeros())])
        .open(
            EtherCrab::builder(r"\Device\NPF_{734CDCA4-E91A-4505-B455-FD6AC42A3EE1}")
                .with_interval(INTERVAL)
                .with_dc_config(DcConfiguration {
                    start_delay: Duration::from_millis(100),
                    sync0_period: INTERVAL,
                    sync0_shift: INTERVAL / 2,
                }),
        )
        .await?;

    autd.send(Silencer::default()).await?;

    let center = autd.geometry().center() + Vector3::new(0., 0., 150.0 * mm);
    let g = Focus::new(center);
    let m = Sine::new(150. * Hz);
    autd.send((m, g)).await?;

    let mut _s = String::new();
    std::io::stdin().read_line(&mut _s)?;

    autd.close().await?;

    Ok(())
}
