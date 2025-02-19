use std::time::Duration;

use anyhow::Result;

use autd3::prelude::*;
use autd3_link_ethercrab::*;

const INTERVAL: Duration = Duration::from_millis(1);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let interfaces = pnet_datalink::interfaces();
    println!("Select interfaces:");
    for (i, interface) in interfaces.iter().enumerate() {
        println!("{}: {} {}", i, interface.name, interface.description);
    }
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    let interface = interfaces[s.trim().parse::<usize>()?].name.clone();

    let mut autd = autd3::r#async::Controller::open(
        [AUTD3::default()],
        EtherCrab::new(
            &interface,
            EtherCrabOption {
                interval: INTERVAL,
                dc_config: DcConfiguration {
                    start_delay: Duration::from_millis(100),
                    sync0_period: INTERVAL,
                    sync0_shift: INTERVAL / 2,
                },
                ..EtherCrabOption::default()
            },
        ),
    )
    .await?;

    autd.send(Silencer::default()).await?;

    let center = autd.center() + Vector3::new(0., 0., 150.0 * mm);
    autd.send((
        Sine {
            freq: 150.0 * Hz,
            option: SineOption::default(),
        },
        Focus {
            pos: center,
            option: FocusOption::default(),
        },
    ))
    .await?;

    let mut _s = String::new();
    std::io::stdin().read_line(&mut _s)?;

    autd.close().await?;

    Ok(())
}
