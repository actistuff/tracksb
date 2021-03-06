//! Enables PMIC (AXP173), turns IMU (BNO080) on and starts to stream
//! current board's position (rotation quaternions) via USB Virtual COM Port.

#![no_main]
#![no_std]
#![allow(non_snake_case)]

use tracksb as _; // global logger + panicking-behavior + memory layout
extern crate stm32wb_hal as hal;

use cortex_m_rt::{exception, ExceptionFrame};
use embedded_hal::blocking::delay::DelayMs;
use hal::{
    delay::DelayCM,
    flash::FlashExt,
    gpio::{ExtiPin, State},
    i2c::I2c,
    prelude::*,
    rcc::{
        ApbDivider, Config, HDivider, HseDivider, PllConfig, PllSrc, Rcc, RfWakeupClock, RtcClkSrc,
        StopWakeupClock, SysClkSrc, UsbClkSrc,
    },
    usb::{Peripheral, UsbBus, UsbBusType},
};
use rtic::{app, export::DWT};
use tracksb::{
    bsp,
    bsp::{ImuIntPin, PmicIntPin, Rgb},
    imu::{ImuWrapper, Quaternion, IMU_REPORTING_INTERVAL_MS, IMU_REPORTING_RATE_HZ},
    pmic,
    pmic::{wait_init_pmic, ImuPowerState, Pmic},
    rgbled::{startup_animate, LedColor, RgbLed},
};
use usb_device::{bus, device::UsbDevice, prelude::*};
use usbd_serial::{SerialPort, USB_CLASS_CDC};

const VCP_TX_BUFFER_SIZE: usize = 32;

#[app(device = stm32wb_hal::pac, peripherals = true, monotonic = rtic::cyccnt::CYCCNT)]
const APP: () = {
    struct Resources {
        usb_dev: UsbDevice<'static, UsbBusType>,
        serial: SerialPort<'static, UsbBusType>,

        pmic: Pmic<pmic::Initialized, hal::i2c::Error, bsp::PmicI2c>,
        pmic_int_pin: PmicIntPin,
        imu: ImuWrapper<hal::i2c::Error, bsp::ImuI2c, bsp::ImuResetPin>,
        imu_int_pin: ImuIntPin,
        delay: DelayCM,

        vcp_tx_buf: [u8; VCP_TX_BUFFER_SIZE],

        rgb_led: Rgb,
    }

    #[init]
    fn init(mut cx: init::Context) -> init::LateResources {
        static mut USB_BUS: Option<bus::UsbBusAllocator<UsbBusType>> = None;

        defmt::info!("Initializing");

        let mut dp = cx.device;

        // Allow using debugger and RTT during WFI/WFE (sleep)
        dp.DBGMCU.cr.modify(|_, w| {
            w.dbg_sleep().set_bit();
            w.dbg_standby().set_bit();
            w.dbg_stop().set_bit()
        });
        dp.RCC.ahb1enr.modify(|_, w| w.dma1en().set_bit());

        let mut rcc: Rcc = dp.RCC.constrain();
        rcc.set_stop_wakeup_clock(StopWakeupClock::MSI);

        // Initialize (enable) the monotonic timer (CYCCNT)
        cx.core.DCB.enable_trace();
        // required on Cortex-M7 devices that software lock the DWT (e.g. STM32F7)
        DWT::unlock();
        cx.core.DWT.enable_cycle_counter();

        // Fastest clock configuration.
        // * External low-speed crystal is used (LSE)
        // * 32 MHz HSE with PLL
        // * 64 MHz CPU1, 32 MHz CPU2
        // * 64 MHz for APB1, APB2
        // * USB clock source from PLLQ (32 / 2 * 3 = 48)
        // * HSI as a clock source after wake-up from low-power mode
        let clock_config = Config::new(SysClkSrc::Pll(PllSrc::Hse(HseDivider::NotDivided)))
            .with_lse()
            .cpu1_hdiv(HDivider::NotDivided)
            .cpu2_hdiv(HDivider::Div2)
            .apb1_div(ApbDivider::NotDivided)
            .apb2_div(ApbDivider::NotDivided)
            .pll_cfg(PllConfig {
                m: 2,
                n: 12,
                r: 3,
                q: Some(4),
                p: Some(3),
            })
            .usb_src(UsbClkSrc::PllQ)
            .rtc_src(RtcClkSrc::Lse)
            .rf_wkp_sel(RfWakeupClock::Lse);

        let mut rcc = rcc.apply_clock_config(clock_config, &mut dp.FLASH.constrain().acr);

        // Enable USB power supply
        hal::pwr::set_usb(true);

        let mut gpioa = dp.GPIOA.split(&mut rcc);
        let mut gpiob = dp.GPIOB.split(&mut rcc);
        let mut delay = hal::delay::DelayCM::new(rcc.clocks);

        let red_led = gpioa.pa4.into_push_pull_output_with_state(
            &mut gpioa.moder,
            &mut gpioa.otyper,
            State::High,
        );
        let green_led = gpioa.pa5.into_push_pull_output_with_state(
            &mut gpioa.moder,
            &mut gpioa.otyper,
            State::High,
        );
        let blue_led = gpioa.pa6.into_push_pull_output_with_state(
            &mut gpioa.moder,
            &mut gpioa.otyper,
            State::High,
        );
        let mut rgb_led = RgbLed::new(red_led, green_led, blue_led);
        startup_animate(&mut rgb_led, &mut delay);

        let usb = Peripheral {
            usb: dp.USB,
            pin_dm: gpioa.pa11.into_af10(&mut gpioa.moder, &mut gpioa.afrh),
            pin_dp: gpioa.pa12.into_af10(&mut gpioa.moder, &mut gpioa.afrh),
        };

        *USB_BUS = Some(UsbBus::new(usb));

        let serial = SerialPort::new(USB_BUS.as_ref().unwrap());

        let usb_dev = UsbDeviceBuilder::new(USB_BUS.as_ref().unwrap(), UsbVidPid(0x16c0, 0x27dd))
            .manufacturer("eupn")
            .product("TrackSB Rev. C")
            .serial_number("TEST")
            .device_class(USB_CLASS_CDC)
            .build();

        /* PMIC */

        // I2C pull-ups are controlled via pin
        let mut pull_ups = gpiob
            .pb5
            .into_push_pull_output(&mut gpiob.moder, &mut gpiob.otyper);
        let _ = pull_ups.set_high();
        let scl = gpiob
            .pb6
            .into_floating_input(&mut gpiob.moder, &mut gpiob.pupdr);
        let sda = gpiob
            .pb7
            .into_floating_input(&mut gpiob.moder, &mut gpiob.pupdr);
        let scl = scl.into_open_drain_output(&mut gpiob.moder, &mut gpiob.otyper);
        let scl = scl.into_af4(&mut gpiob.moder, &mut gpiob.afrl);
        let sda = sda.into_open_drain_output(&mut gpiob.moder, &mut gpiob.otyper);
        let sda = sda.into_af4(&mut gpiob.moder, &mut gpiob.afrl);
        let mut pmic = wait_init_pmic(dp.I2C1, scl, sda, &mut rcc, &mut delay);
        pmic.set_imu_power(true).unwrap();
        let pmic_int_pin = bsp::init_pmic_interrupt(
            gpiob
                .pb1
                .into_pull_up_input(&mut gpiob.moder, &mut gpiob.pupdr),
            &mut dp.SYSCFG,
            &mut dp.EXTI,
        );

        /* IMU */

        let mut imu_rst = gpioa
            .pa15
            .into_push_pull_output(&mut gpioa.moder, &mut gpioa.otyper);
        imu_rst.set_low().unwrap();
        delay.delay_ms(100_u16);
        imu_rst.set_high().unwrap();

        let imu_int_pin = bsp::init_imu_interrupt(
            gpiob
                .pb3
                .into_pull_up_input(&mut gpiob.moder, &mut gpiob.pupdr),
            &mut dp.SYSCFG,
            &mut dp.EXTI,
        );

        let scl = gpioa
            .pa7
            .into_floating_input(&mut gpioa.moder, &mut gpioa.pupdr);
        let scl = scl.into_open_drain_output(&mut gpioa.moder, &mut gpioa.otyper);
        let scl = scl.into_af4(&mut gpioa.moder, &mut gpioa.afrl);
        let sda = gpiob
            .pb4
            .into_open_drain_output(&mut gpiob.moder, &mut gpiob.otyper);
        let sda = sda.into_af4(&mut gpiob.moder, &mut gpiob.afrl);
        let i2c3 = I2c::i2c3(dp.I2C3, (scl, sda), 100.khz(), &mut rcc);

        let imu = ImuWrapper::new(i2c3, imu_rst);

        defmt::info!(
            "Initialized MCU at {:u32} MHz and IMU at {:u16} Hz",
            rcc.clocks.sysclk().0 / 1000,
            IMU_REPORTING_RATE_HZ
        );

        init::LateResources {
            usb_dev,
            serial,
            pmic,
            pmic_int_pin,
            imu,
            imu_int_pin,
            delay,
            vcp_tx_buf: [0u8; VCP_TX_BUFFER_SIZE],
            rgb_led,
        }
    }

    #[task(resources = [vcp_tx_buf, serial], capacity = 4)]
    fn vcp_tx(cx: vcp_tx::Context, quat: Quaternion) {
        let mut buf = [0u8; 128];
        let _s: &str = write_to::show(
            &mut buf,
            format_args!("{} {} {} {}\n", quat[0], quat[1], quat[2], quat[3]),
        )
        .unwrap();

        // Ignore the result
        let _ = cx.resources.serial.write(&buf[..]);
    }

    #[task(binds = USB_HP, resources = [usb_dev, serial])]
    fn usb_tx(cx: usb_tx::Context) {
        cx.resources.usb_dev.poll(&mut [cx.resources.serial]);
    }

    #[task(binds = USB_LP, resources = [usb_dev, serial])]
    fn usb_rx0(cx: usb_rx0::Context) {
        let usbdev: &mut UsbDevice<'static, UsbBusType> = cx.resources.usb_dev;
        if !usbdev.poll(&mut [cx.resources.serial]) {
            return;
        }

        let mut buf = [0u8; 32];
        cx.resources.serial.read(&mut buf[..]).ok();
    }

    #[task(resources = [imu, delay, rgb_led], spawn = [vcp_tx], capacity = 2)]
    fn poll_imu(cx: poll_imu::Context) {
        if let Some(quat) = cx.resources.imu.quaternion(cx.resources.delay).unwrap() {
            cx.resources.rgb_led.toggle(LedColor::Green);
            cx.spawn.vcp_tx(quat).unwrap();
        }
    }

    #[task(binds = EXTI3, resources = [delay, imu, imu_int_pin, pmic], spawn = [poll_imu])]
    fn imu_interrupt(cx: imu_interrupt::Context) {
        let int_pin = cx.resources.imu_int_pin;

        if int_pin.check_interrupt() {
            int_pin.clear_interrupt_pending_bit();

            // Ignore interrupts if IMU isn't enabled
            if !cx.resources.pmic.imu_enabled().unwrap() {
                return;
            }

            // Initialize the IMU if it just booted up
            if !cx.resources.imu.is_initialized() {
                defmt::info!("BNO08x booted, initializing...");
                cx.resources
                    .imu
                    .init_imu(cx.resources.delay, IMU_REPORTING_INTERVAL_MS);
            } else {
                cx.spawn.poll_imu().unwrap();
            }
        }
    }

    #[task(binds = EXTI1, resources = [pmic_int_pin, pmic, imu, delay], spawn = [imu_on_off])]
    fn pmic_interrupt(cx: pmic_interrupt::Context) {
        let int_pin = cx.resources.pmic_int_pin;

        if int_pin.check_interrupt() {
            int_pin.clear_interrupt_pending_bit();

            // Process IRQs and manage IMU power if it was a power button IRQ
            let imu_power_state = cx.resources.pmic.process_irqs().unwrap();
            match imu_power_state {
                ImuPowerState::Shutdown => cx.spawn.imu_on_off(false).unwrap(),
                ImuPowerState::Enabled => cx.spawn.imu_on_off(true).unwrap(),
                ImuPowerState::Unchanged => (),
            }

            cx.resources.pmic.show_current().unwrap();
        }
    }

    #[task(resources = [imu, delay, rgb_led])]
    fn imu_on_off(cx: imu_on_off::Context, turn_on: bool) {
        if turn_on {
            if !cx.resources.imu.is_initialized() {
                cx.resources.imu.reset_imu(cx.resources.delay);
            }
        } else {
            cx.resources.imu.deinit();
            cx.resources.rgb_led.turn_off_all();
            // TODO: put the MCU into deep sleep
        }
    }

    // Interrupt handlers used to dispatch software tasks
    extern "C" {
        fn USART1();
    }
};

#[exception]
fn DefaultHandler(irqn: i16) -> ! {
    panic!("Unhandled IRQ: {}", irqn);
}

#[exception]
unsafe fn HardFault(_ef: &ExceptionFrame) -> ! {
    // Turn the red led on and turn the others off
    let mut rcc = Rcc {
        clocks: Default::default(),
        config: Default::default(),
        rb: hal::pac::Peripherals::steal().RCC,
    };
    let mut gpioa = hal::pac::Peripherals::steal().GPIOA.split(&mut rcc);
    let mut red_led = gpioa
        .pa4
        .into_push_pull_output(&mut gpioa.moder, &mut gpioa.otyper);
    let mut green_led = gpioa
        .pa5
        .into_push_pull_output(&mut gpioa.moder, &mut gpioa.otyper);
    let mut blue_led = gpioa
        .pa6
        .into_push_pull_output(&mut gpioa.moder, &mut gpioa.otyper);

    red_led.set_low().unwrap();
    green_led.set_high().unwrap();
    blue_led.set_high().unwrap();

    cortex_m::asm::udf();
}

pub mod write_to {
    use core::{cmp::min, fmt};

    pub struct WriteTo<'a> {
        buffer: &'a mut [u8],
        // on write error (i.e. not enough space in buffer) this grows beyond
        // `buffer.len()`.
        used: usize,
    }

    impl<'a> WriteTo<'a> {
        pub fn new(buffer: &'a mut [u8]) -> Self {
            WriteTo { buffer, used: 0 }
        }

        pub fn into_str(self) -> Option<&'a str> {
            if self.used <= self.buffer.len() {
                // only successful concats of str - must be a valid str.
                use core::str::from_utf8_unchecked;
                Some(unsafe { from_utf8_unchecked(&self.buffer[..self.used]) })
            } else {
                None
            }
        }
    }

    impl<'a> fmt::Write for WriteTo<'a> {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            if self.used > self.buffer.len() {
                return Err(fmt::Error);
            }
            let remaining_buf = &mut self.buffer[self.used..];
            let raw_s = s.as_bytes();
            let write_num = min(raw_s.len(), remaining_buf.len());
            remaining_buf[..write_num].copy_from_slice(&raw_s[..write_num]);
            self.used += raw_s.len();
            if write_num < raw_s.len() {
                Err(fmt::Error)
            } else {
                Ok(())
            }
        }
    }

    pub fn show<'a>(buffer: &'a mut [u8], args: fmt::Arguments) -> Result<&'a str, fmt::Error> {
        let mut w = WriteTo::new(buffer);
        fmt::write(&mut w, args)?;
        w.into_str().ok_or(fmt::Error)
    }
}
