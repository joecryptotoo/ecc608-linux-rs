use bytes::{BufMut, BytesMut};
use std::{fs::File, thread, time::Duration, env};
use lazy_static::lazy_static;
#[cfg(feature = "raspi")]
use rppal::{gpio::Gpio, gpio::Mode, system::DeviceInfo};

use crate::constants::{
    ATCA_I2C_COMMAND_FLAG, ATCA_RSP_SIZE_MAX, ATCA_SWI_COMMAND_FLAG, ATCA_SWI_IDLE_FLAG,
    ATCA_SWI_SLEEP_FLAG, ATCA_SWI_TRANSMIT_FLAG,
};
use crate::{Error, Result};

use i2c_linux::I2c;
use serialport::{ClearBuffer, SerialPort};

const DEFAULT_SCL_PIN: u8 = 3; // Replace with your default SCL pin
const DEFAULT_SDA_PIN: u8 = 2; // Replace with your default SDA pin

const RECV_RETRY_WAIT: Duration = Duration::from_millis(4);
const RECV_RETRY_WAIT_LAST_RESORT: Duration = Duration::from_millis(40);
const RECV_RETRIES: u8 = 10;
const SWI_DEFAULT_BAUDRATE: u32 = 230_400;
const SWI_WAKE_BAUDRATE: u32 = 115_200;
const SWI_BIT_SEND_DELAY: Duration = Duration::from_micros(45);

#[cfg(feature = "raspi")]
lazy_static! { static ref IS_RASPI: bool = rppal::system::DeviceInfo::new().is_ok(); }

#[cfg(not(feature = "raspi"))]
lazy_static! { static ref IS_RASPI: bool = false; }

pub struct I2cTransport {
    port: I2c<File>,
    address: u16,
}

pub struct SwiTransport {
    port: Box<dyn SerialPort>,
}
pub(crate) enum TransportProtocol {
    I2c(I2cTransport),
    Swi(SwiTransport),
}

impl From<I2cTransport> for TransportProtocol {
    fn from(i2c_handle: I2cTransport) -> Self {
        Self::I2c(i2c_handle)
    }
}

impl From<SwiTransport> for TransportProtocol {
    fn from(swi_handle: SwiTransport) -> Self {
        Self::Swi(swi_handle)
    }
}

#[cfg(feature = "raspi")]
impl From<rppal::gpio::Error> for Error {
    fn from(err: rppal::gpio::Error) -> Self {
        Error::timeout()
    }
}

impl TransportProtocol {
    pub fn send_wake(&mut self, wake_delay: Duration, wake_duration: Duration) -> Result {
        match self {
            Self::I2c(i2c_handle) => i2c_handle.send_wake(wake_delay, wake_duration),
            Self::Swi(swi_handle) => swi_handle.send_wake(wake_delay),
        }
    }

    pub fn send_idle(&mut self) {
        match self {
            Self::I2c(i2c_handle) => i2c_handle.send_idle(),
            Self::Swi(swi_handle) => swi_handle.send_idle(),
        }
    }

    pub fn send_sleep(&mut self) {
        match self {
            Self::I2c(i2c_handle) => i2c_handle.send_sleep(),
            Self::Swi(swi_handle) => swi_handle.send_sleep(),
        }
    }

    pub fn send_recv_buf(&mut self, delay: Duration, buf: &mut BytesMut) -> Result {
        match self {
            Self::I2c(i2c_handle) => i2c_handle.send_recv_buf(delay, buf),
            Self::Swi(swi_handle) => swi_handle.send_recv_buf(delay, buf),
        }
    }

    pub fn put_command_flag(&self) -> u8 {
        match self {
            Self::I2c(_) => ATCA_I2C_COMMAND_FLAG,
            Self::Swi(_) => ATCA_SWI_COMMAND_FLAG,
        }
    }
}

impl I2cTransport {
    pub fn new(path: &str, address: u16) -> Result<Self> {
        let mut port = I2c::from_path(path)?;
        port.smbus_set_slave_address(address, false)?;
        // port.i2c_set_retries(RECV_RETRIES as usize);
        // port.i2c_set_timeout(RECV_RETRY_WAIT);

        Ok(Self { port, address })
    }

    fn send_wake(&mut self, wake_delay: Duration, wake_duration: Duration) -> Result {
        if *IS_RASPI {

            let scl_pin_number: u8 = env::var("GW_SCL_PIN")
                .unwrap_or_else(|_| DEFAULT_SCL_PIN.to_string())
                .parse()
                .unwrap_or(DEFAULT_SCL_PIN);

            let sda_pin_number: u8 = env::var("GW_SDA_PIN")
                .unwrap_or_else(|_| DEFAULT_SDA_PIN.to_string())
                .parse()
                .unwrap_or(DEFAULT_SDA_PIN);
            
            #[cfg(feature = "raspi")]
            {
                // Create a new Gpio instance
                let gpio = Gpio::new()?;

                // Retrieve the SDA and SCL pins as output pins
                let mut sda_pin = gpio.get(sda_pin_number)?.into_output();
                let mut scl_pin = gpio.get(scl_pin_number)?.into_output();

                // Send the wake pulse
                sda_pin.set_low();
                scl_pin.set_low();

                // Hold them low for 60 microseconds
                thread::sleep(wake_duration);

                sda_pin.set_high();
                scl_pin.set_high();

                // Drop pins
                drop(sda_pin);
                drop(scl_pin);
            }

            #[cfg(not(feature = "raspi"))]
            { let _ = self.send_buf(0, &[0x00]); }

        } else {
            let _ = self.send_buf(0, &[0x00]);
        }
        thread::sleep(wake_delay);
        Ok(())
    }

    fn send_idle(&mut self) {
        let _ = self.send_buf(self.address, &[0x02]);
    }

    fn send_sleep(&mut self) {
        let _ = self.send_buf(self.address, &[0x01]);
    }

    fn send_recv_buf(&mut self, delay: Duration, buf: &mut BytesMut) -> Result {
        self.send_buf(self.address, &buf[..])?;
        thread::sleep(delay);
        self.recv_buf(buf)
    }

    fn send_buf(&mut self, address: u16, buf: &[u8]) -> Result {
        let write_msg = i2c_linux::Message::Write {
            address,
            data: buf,
            flags: Default::default(),
        };

        self.port.i2c_transfer(&mut [write_msg])?;
        Ok(())
    }

    fn recv_buf(&mut self, buf: &mut BytesMut) -> Result {
        buf.resize(ATCA_RSP_SIZE_MAX as usize, 0);
        buf[0] = 0xff;
        for retry in 0..=RECV_RETRIES {
            if retry == RECV_RETRIES {
                thread::sleep(RECV_RETRY_WAIT_LAST_RESORT);
            }
            let msg = i2c_linux::Message::Read {
                address: self.address,
                data: buf,
                flags: Default::default(),
            };
            if self.port.i2c_transfer(&mut [msg]).is_ok() {
                break;
            }
            thread::sleep(RECV_RETRY_WAIT);
        }
        let count = buf[0] as usize;
        if count == 0xff {
            // Sleep the chip to clear the SRAM when the maximum error read retries have been exhausted
            self.send_sleep();
            return Err(Error::timeout());
        }
        buf.truncate(count);
        Ok(())
    }
}

impl SwiTransport {
    pub fn new(path: &str) -> Result<Self> {
        let port = serialport::new(path, SWI_DEFAULT_BAUDRATE)
            .data_bits(serialport::DataBits::Seven)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .timeout(Duration::from_millis(50))
            .open()?;

        Ok(Self { port })
    }

    fn send_wake(&mut self, wake_delay: Duration) -> Result {
        if let Err(_err) = self.port.as_mut().set_baud_rate(SWI_WAKE_BAUDRATE) {
            return Err(Error::timeout());
        }

        let _ = self.port.as_mut().write(&[0]);

        thread::sleep(wake_delay);
        let _ = self.port.as_mut().set_baud_rate(SWI_DEFAULT_BAUDRATE);
        let _ = self.port.as_mut().clear(ClearBuffer::All);
        Ok(())
    }

    fn send_idle(&mut self) {
        let idle_encoded = self.encode_uart_to_swi(&[ATCA_SWI_IDLE_FLAG]);
        let _ = self.port.as_mut().write(&idle_encoded);
        thread::sleep(SWI_BIT_SEND_DELAY * 8);
    }

    fn send_sleep(&mut self) {
        let sleep_encoded = self.encode_uart_to_swi(&[ATCA_SWI_SLEEP_FLAG]);
        let _ = self.port.as_mut().write(&sleep_encoded);
        thread::sleep(SWI_BIT_SEND_DELAY * 8);
    }

    fn send_recv_buf(&mut self, delay: Duration, buf: &mut BytesMut) -> Result {
        let _ = self.port.as_mut().clear(ClearBuffer::All);
        let swi_msg = self.encode_uart_to_swi(buf);
        self.send_swi_buf(&swi_msg)?;
        thread::sleep(delay);
        self.recv_swi_buf(buf)
    }

    fn send_swi_buf(&mut self, buf: &[u8]) -> Result {
        let send_size = self.port.as_mut().write(buf)?;

        //Each byte takes ~45us to transmit, so we must wait for the transmission to finish before proceeding
        let uart_tx_time = buf.len() as u32 * SWI_BIT_SEND_DELAY;
        thread::sleep(uart_tx_time);
        //Because Tx line is linked with Rx line, all sent msgs are returned on the Rx line and must be cleared from the buffer
        let mut clear_rx_line = BytesMut::new();
        clear_rx_line.resize(send_size, 0);
        let _ = self.port.as_mut().read_exact(&mut clear_rx_line);

        Ok(())
    }

    fn recv_swi_buf(&mut self, buf: &mut BytesMut) -> Result {
        buf.resize(2, 0xFF);
        buf[1] = 0xFF;

        let encoded_transmit_flag = self.encode_uart_to_swi(&[ATCA_SWI_TRANSMIT_FLAG]);

        let _ = self.port.as_mut().clear(ClearBuffer::All);

        for _retry in 0..RECV_RETRIES {
            self.port.as_mut().write_all(&encoded_transmit_flag)?;

            if let Err(_err) = self.decode_swi_to_uart(&mut buf[0..2]) {
            } else {
                break;
            }
            thread::sleep(RECV_RETRY_WAIT);
        }

        let _ = buf.split_to(1); // Discard transmit flag

        let count = buf[0] as usize;
        if count == 0xFF {
            return Err(Error::timeout());
        }
        buf.resize(count, 0);
        if let Err(_err) = self.decode_swi_to_uart(&mut buf[1..count]) {
            return Err(Error::timeout());
        }
        Ok(())
    }

    fn encode_uart_to_swi(&mut self, uart_msg: &[u8]) -> BytesMut {
        let mut bit_field = BytesMut::with_capacity(uart_msg.len() * 8);

        for byte in uart_msg.iter() {
            for bit_index in 0..8 {
                if (((1 << bit_index) & byte) >> bit_index) == 0 {
                    bit_field.put_u8(0xFD);
                } else {
                    bit_field.put_u8(0xFF);
                }
            }
        }
        bit_field
    }

    fn decode_swi_to_uart(&mut self, buf: &mut [u8]) -> Result {
        for byte in buf {
            let mut decoded_byte = 0;
            let mut bit_mask: u8 = 1;

            while bit_mask != 0 {
                let mut rx_byte = [0; 1];

                if let Ok(_rx_count) = self.port.as_mut().read(&mut rx_byte) {
                    if (rx_byte[0] ^ 0x7F) < 2 {
                        decoded_byte |= bit_mask;
                    }
                } else {
                    return Err(Error::timeout());
                }
                bit_mask <<= 1;
            }

            *byte = decoded_byte;
        }
        Ok(())
    }
}
