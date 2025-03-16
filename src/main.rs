//! Raspberry Pi Pico W meat thermometer

#![no_std]
#![no_main]
// required for impl in AppProps code for picoserve
#![feature(impl_trait_in_assoc_type)]

use cyw43::JoinOptions;
use cyw43_pio::{PioSpi, DEFAULT_CLOCK_DIVIDER};
use embassy_executor::Spawner;
use embassy_net::{StackResources, Ipv4Cidr, Ipv4Address};
use embassy_rp::bind_interrupts;
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::i2c::{self, Config};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::peripherals::USB;
use embassy_rp::peripherals::I2C1;
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use embassy_rp::usb::{Driver, InterruptHandler as USBInterruptHandler};
use embassy_time::{Duration, Timer, Delay};
use heapless::Vec;
use libscd::synchronous::scd4x::Scd4x;
use picoserve::{
    make_static,
    routing::{get, get_service, PathRouter},
    AppWithStateBuilder, AppRouter,
    response::DebugValue
};
use picoserve::response::File;
use picoserve::extract::State;
use rand::RngCore;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

const WIFI_NETWORK: &str = "bendybogalow";
const WIFI_PASSWORD: &str = "parsnipcabbageonion";
const INDEX: &str = include_str!("html/index.html");

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
});

bind_interrupts!(struct UsbIrqs {
    USBCTRL_IRQ => USBInterruptHandler<USB>;
});

#[embassy_executor::task]
async fn logger_task(driver: Driver<'static, USB>) {
    embassy_usb_logger::run!(1024, log::LevelFilter::Info, driver);
}

#[embassy_executor::task]
async fn cyw43_task(runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

/*
async fn co2ppm_to_str {
    let mut buff : [u8; 20] = [0u8; 20];
    unsafe {
        CO2PPM.numtoa(10, &mut buff);
    }
    let buffstr: &str = core::str::from_utf8(&mut buff).unwrap();
    log::info!("CO2PPM from build_app: {}", buffstr);
    "1234" // works
    // buffstr // fails for the obvious reason of lifetime
}
*/
/*
impl Content for AtomicU16 {
    fn content_type(&self) -> &'static str {
        "text/plain; charset=utf-8"
    }
    fn content_length(&self) -> usize {
        5;
    }
    async fn write_content<W: Write>(self, writer: W) -> Result<(), W::Error> {
        "fooo".as_bytes().write_content(writer).await
    }
}
*/
/*
struct Number {
    value: u16;
}

async fn get_number(Number { value }: Number) -> impl IntoResponse {
    picoserve::response::DebugValue(value)
}
*/

struct CO2PPM {
    co2ppm : u16,
}
#[derive(Clone, Copy)]
struct SharedPPM(&'static Mutex<CriticalSectionRawMutex, CO2PPM>);
struct AppState {
    shared_ppm : SharedPPM,
}
impl picoserve::extract::FromRef<AppState> for SharedPPM {
    fn from_ref(state: &AppState) -> Self {
        state.shared_ppm
    }
}

// picoserve HTTP code kicked off using: https://github.com/sammhicks/picoserve/blob/main/examples/embassy/hello_world/src/main.rs
struct AppProps;

impl AppWithStateBuilder for AppProps {
    type State = AppState;
    type PathRouter = impl PathRouter<AppState>;

    fn build_app(self) -> picoserve::Router<Self::PathRouter, Self::State> {
        //let Self { } = self;

        /*let mut buff : [u8; 20] = [0u8; 20];
        unsafe {
            let _ = CO2PPM.numtoa(10, &mut buff);
        }
        let buffstr: &str = core::str::from_utf8(&mut buff).unwrap();
        log::info!("CO2PPM from build_app: {}", buffstr);*/

        picoserve::Router::new()
            .route(
                "/", 
                get_service(File::html(INDEX)) // .replace("%{CO2}%", CO2PPM.to_string())))
            )
            .route(
                "/main.css", 
                get_service(File::css(include_str!("html/main.css")))
            )
            .route(
                "/data/co2", 
                get(|State(SharedPPM(co2ppm)): State<SharedPPM>| async move { DebugValue( co2ppm.lock().await.co2ppm ) }),
            )
    }
}

// 2 is plenty of a little IoT thermometer, right?
const WEB_TASK_POOL_SIZE: usize = 2;

#[embassy_executor::task(pool_size = WEB_TASK_POOL_SIZE)]
async fn web_task(
    id: usize,
    stack: embassy_net::Stack<'static>,
    app: &'static AppRouter<AppProps>,
    config: &'static picoserve::Config<Duration>,
    state: AppState,
) -> ! {
    let port = 80;
    let mut tcp_rx_buffer = [0; 1024];
    let mut tcp_tx_buffer = [0; 1024];
    let mut http_buffer = [0; 2048];

    picoserve::listen_and_serve_with_state(
        id,
        app,
        config,
        stack,
        port,
        &mut tcp_rx_buffer,
        &mut tcp_tx_buffer,
        &mut http_buffer,
        &state,
    )
    .await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let driver = Driver::new(p.USB, UsbIrqs);
    spawner.spawn(logger_task(driver)).unwrap();
    let mut rng = RoscRng;

    // TODO: this was required to make log entries before the loop actually reach the TTY - so I am
    // guessing there is some setup happening in the background and wonder if there is a better way
    // to wait for that than a sleep...
    Timer::after_secs(1).await;

    log::info!("main: entry");

    let fw = include_bytes!("../cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../cyw43-firmware/43439A0_clm.bin");

    // To make flashing faster for development, you may want to flash the firmwares independently
    // at hardcoded addresses, instead of baking them into the program with `include_bytes!`:
    //     probe-rs download ../../cyw43-firmware/43439A0.bin --binary-format bin --chip RP2040 --base-address 0x10100000
    //     probe-rs download ../../cyw43-firmware/43439A0_clm.bin --binary-format bin --chip RP2040 --base-address 0x10140000
    //let fw = unsafe { core::slice::from_raw_parts(0x10100000 as *const u8, 230321) };
    //let clm = unsafe { core::slice::from_raw_parts(0x10140000 as *const u8, 4752) };

    log::info!("main: init IO");
    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        DEFAULT_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        p.DMA_CH0,
    );

    log::info!("main: init wifi");
    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    defmt::unwrap!(spawner.spawn(cyw43_task(runner)));

    log::info!("main: init clm");
    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    //let config = Config::dhcpv4(Default::default());
    log::info!("main: configure static IP");
    let config = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: Ipv4Cidr::new(Ipv4Address::new(192, 168, 3, 15), 24),
        dns_servers: Vec::new(),
        gateway: Some(Ipv4Address::new(192, 168, 3, 1)),
    });
    
    // Generate random seed
    let seed = rng.next_u64();

    // Init network stack
    log::info!("main: init network stack");
    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(net_device, config, RESOURCES.init(StackResources::new()), seed);

    defmt::unwrap!(spawner.spawn(net_task(runner)));

    log::info!("main: await network join");
    loop {
        match control
            .join(WIFI_NETWORK, JoinOptions::new(WIFI_PASSWORD.as_bytes()))
            .await
        {
            Ok(_) => break,
            Err(err) => {
                log::error!("join failed with status={}", err.status);
            }
        }
        Timer::after_millis(100).await;
    }

    // Wait for DHCP, not necessary when using static IP
    /*log::info!("waiting for DHCP...");
    while !stack.is_config_up() {
        Timer::after_millis(100).await;
    }
    log::info!("DHCP is now up!");*/


    log::info!("Starting I2C Comms with SCD40");
    Timer::after_secs(1).await;
    // this code derived from: https://github.com/SvetlinZarev/libscd/blob/main/examples/embassy-scd4x/src/main.rs
    let sda = p.PIN_26;
    let scl = p.PIN_27;
    let i2c = i2c::I2c::new_blocking(p.I2C1, scl, sda, Config::default());
    log::info!("Initialise Scd4x");
    Timer::after_secs(1).await;
    let mut scd = Scd4x::new(i2c, Delay);

    // When re-programming, the controller will be restarted,
    // but not the sensor. We try to stop it in order to
    // prevent the rest of the commands failing.
    log::info!("Stop periodic measurements");
    Timer::after_secs(1).await;
    _ = scd.stop_periodic_measurement();

    log::info!("Sensor serial number: {:?}", scd.serial_number());
    Timer::after_secs(1).await;
    if let Err(e) = scd.start_periodic_measurement() {
        log::error!("Failed to start periodic measurement: {:?}", e );
    }

    let co2ppm = 69;
    let shared_ppm = SharedPPM(
        make_static!(Mutex<CriticalSectionRawMutex, CO2PPM>, Mutex::new(CO2PPM { co2ppm })),
    ); 
    spawner.must_spawn(read_co2(scd, shared_ppm));

    log::info!("Commence HTTP service");
    Timer::after_secs(1).await;

    let app = make_static!(AppRouter<AppProps>, AppProps.build_app());

    let config = make_static!(
        picoserve::Config<Duration>,
        picoserve::Config::new(picoserve::Timeouts {
            start_read_request: Some(Duration::from_secs(5)),
            read_request: Some(Duration::from_secs(1)),
            write: Some(Duration::from_secs(1)),
        })
        .keep_connection_alive()
    );

    for id in 0..WEB_TASK_POOL_SIZE {
        spawner.must_spawn(web_task(
            id,
            stack,
            app,
            config,
            AppState{ shared_ppm },
        ));
    }
/*
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];
    let mut buf = [0; 4096];

    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);

    //let delay = Duration::from_secs(1);
    log::info!("main: pre-loop");
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));

        control.gpio_set(0, false).await;
        log::info!("Listening on TCP:00...");
        if let Err(e) = socket.accept(1234).await {
            log::warn!("accept error: {:?}", e);
            continue;
        }

        log::info!("Received connection from {:?}", socket.remote_endpoint());

        control.gpio_set(0, true).await;

        loop {
            let n = match socket.read(&mut buf).await {
                Ok(0) => {
                    log::warn!("read EOF");
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    log::warn!("read error: {:?}", e);
                    break;
                }
            };

            log::info!("rxd {}", from_utf8(&buf[..n]).unwrap());

            match socket.write_all(&buf[..n]).await {
                Ok(()) => {}
                Err(e) => {
                    log::warn!("write error: {:?}", e);
                    break;
                }
            };

        }

        log::info!("LED off!");
        control.gpio_set(0, false).await;
    }
    */
}

#[embassy_executor::task]
async fn read_co2(
    mut scd: Scd4x<i2c::I2c<'static, I2C1, i2c::Blocking>, Delay>,
    shared_ppm: SharedPPM
) {
    log::info!("Enter sensor read loop");
    Timer::after_secs(1).await;
    loop {
        if scd.data_ready().unwrap() {
            let m = scd.read_measurement().unwrap();
            shared_ppm.0.lock().await.co2ppm = m.co2;
            log::info!(
                "CO2: {}\nHumidity: {}\nTemperature: {}", m.co2, m.humidity, m.temperature
            )
        }
        Timer::after_secs(1).await;
    }
}

