//! scd40-rusty-pi: Raspberry Pi Pico W CO2 PPM Monitor with Web Interface

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
use embassy_rp::peripherals::I2C1;
use embassy_rp::peripherals::USB;
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::usb::{Driver, InterruptHandler as USBInterruptHandler};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use embassy_time::{Duration, Timer, Delay};
use heapless::Vec;
use libscd::synchronous::scd4x::Scd4x;
use picoserve::extract::State;
use picoserve::{ make_static, routing::{get, get_service, PathRouter}, AppWithStateBuilder, AppRouter };
use picoserve::response::File;
use rand::RngCore;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

// oh no, exposing my IoT WiFi credentials to the world ;)
const WIFI_NETWORK: &str = "bendybogalow";
const WIFI_PASSWORD: &str = "parsnipcabbageonion";
const INDEX: &str = include_str!("html/index.html");
const CSS: &str = include_str!("html/main.css");

// TODO: I think these calls can be combined?
bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
});
bind_interrupts!(struct UsbIrqs {
    USBCTRL_IRQ => USBInterruptHandler<USB>;
});

// from example code
#[embassy_executor::task]
async fn logger_task(driver: Driver<'static, USB>) {
    embassy_usb_logger::run!(1024, log::LevelFilter::Info, driver);
}

// from example code
#[embassy_executor::task]
async fn cyw43_task(runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>) -> ! {
    runner.run().await
}

// from example code
#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}


//////////////////////////////////////////////////////////////////////
// Shared state related code

// an instance of this will be shared between the sensor read task and the web serve task
struct SCD40Data {
    co2ppm : u16,
    humidity : f32,
    temperature : f32,
}

// some sort of mutex wrapper for a SCD40Data instance?
#[derive(Clone, Copy)]
struct SharedSCD40Data(&'static Mutex<CriticalSectionRawMutex, SCD40Data>);

// this then allows us to pass the shared SCD40Data into the picoserve interface
struct AppState {
    shared_scd40data : SharedSCD40Data,
}
// this is used in the route closure below to extract the SharedSCD40Data struct from the app state
impl picoserve::extract::FromRef<AppState> for SharedSCD40Data {
    fn from_ref(state: &AppState) -> Self {
        state.shared_scd40data
    }
}


//////////////////////////////////////////////////////////////////////
// HTTP handler code

struct AppProps;

impl AppWithStateBuilder for AppProps {
    type State = AppState;
    type PathRouter = impl PathRouter<AppState>;

    fn build_app(self) -> picoserve::Router<Self::PathRouter, Self::State> {
        picoserve::Router::new()
            .route(
                "/", 
                get_service(File::html(INDEX)),
            )
            .route(
                "/main.css", 
                get_service(File::css(CSS)),
            )
            .route(
                "/data", 
                get(
                    |State(SharedSCD40Data(scd40data)): State<SharedSCD40Data>| //newbie note: | delimits a closure
                    async move { picoserve::response::Json(
                            (
                                // this generates JSON like [[a,b],[c,d],[e,f]]
                                // TODO: I'd rather a map but havent found a way to do that
                                ("co2ppm", scd40data.lock().await.co2ppm),
                                ("temperature", scd40data.lock().await.temperature),
                                ("humidity", scd40data.lock().await.humidity),
                            )
                        )
                    }
                ),
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


//////////////////////////////////////////////////////////////////////
// CO2 Sensor Code

#[embassy_executor::task]
async fn read_co2(
    mut scd: Scd4x<i2c::I2c<'static, I2C1, i2c::Blocking>, Delay>,
    shared_scd40data: SharedSCD40Data
) {
    log::info!("Enter sensor read loop");
    loop {
        if scd.data_ready().unwrap() {
            let m = scd.read_measurement().unwrap();
            // TODO: is there a way to write this in one block/struct rather than three locks?
            shared_scd40data.0.lock().await.co2ppm = m.co2;
            shared_scd40data.0.lock().await.temperature = m.temperature;
            shared_scd40data.0.lock().await.humidity = m.humidity;
            log::info!(
                "CO2: {}\nHumidity: {}\nTemperature: {}", m.co2, m.humidity, m.temperature
            )
        }
        Timer::after_secs(1).await;
    }
}


//////////////////////////////////////////////////////////////////////
// main!

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let driver = Driver::new(p.USB, UsbIrqs);

    spawner.spawn(logger_task(driver)).unwrap();
    log::info!("main: entry");

    // TODO: this was required to make log entries before the loop actually reach the TTY - so I am
    // guessing there is some setup happening in the background and wonder if there is a better way
    // to wait for TTY to be ready that than a sleep... (the sleep here is not in fact a 100%
    // reliable way to ensure the TTY is ready it seems, but works most of the time.)
    Timer::after_secs(1).await;

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


    ////////////////////////////////////////////
    // get the WiFi up

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


    ////////////////////////////////////////////
    // get an IP sorted

    // if DHCP then use this code:
    //let config = embassy_net::Config::dhcpv4(Default::default());
    // if static IP then use this code:
    log::info!("main: configure static IP");
    let config = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: Ipv4Cidr::new(Ipv4Address::new(192, 168, 1, 113), 24),
        dns_servers: Vec::new(),
        gateway: Some(Ipv4Address::new(192, 168, 1, 254)),
    });
    
    // Generate random seed
    let mut rng = RoscRng;
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
    log::info!("DHCP is now up!");
    */

    ////////////////////////////////////////////
    // Shared state required by our to main tasks (sensor reader, web server)

    let scd40data = SCD40Data { co2ppm: 0, humidity: 0.0, temperature: 0.0 };
    let shared_scd40data = SharedSCD40Data(
        make_static!(Mutex<CriticalSectionRawMutex, SCD40Data>, Mutex::new( scd40data )),
    ); 


    ////////////////////////////////////////////
    // Set up the SCD40 I2C sensor
    
    log::info!("Starting I2C Comms with SCD40");
    // this code derived from: https://github.com/SvetlinZarev/libscd/blob/main/examples/embassy-scd4x/src/main.rs
    // TODO: how to make pins configurable?
    let sda = p.PIN_26;
    let scl = p.PIN_27;
    let i2c = i2c::I2c::new_blocking(p.I2C1, scl, sda, Config::default());
    log::info!("Initialise Scd4x");
    let mut scd = Scd4x::new(i2c, Delay);

    // When re-programming, the controller will be restarted, but not the sensor. We try to stop it
    // in order to prevent the rest of the commands failing.
    log::info!("Stop periodic measurements");
    _ = scd.stop_periodic_measurement();

    log::info!("Sensor serial number: {:?}", scd.serial_number());
    if let Err(e) = scd.start_periodic_measurement() {
        log::error!("Failed to start periodic measurement: {:?}", e );
    
    }

    spawner.must_spawn(read_co2(scd, shared_scd40data));

    ////////////////////////////////////////////
    // Set up the HTTP service
    
    log::info!("Commence HTTP service");

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
            AppState{ shared_scd40data },
        ));
    }
}

