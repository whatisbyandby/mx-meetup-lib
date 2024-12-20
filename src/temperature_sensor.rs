use embassy_rp::adc::{Adc, Async, Channel};


pub trait TemperatureSensor {
	async fn read_temperature(&mut self) -> f32;
}

pub struct BuiltInTemperatureSensor<'a> {
	adc: Adc<'a, Async>,
	ts: Channel<'a>
}

impl <'a>BuiltInTemperatureSensor<'a> {

	fn convert_to_celsius(raw_temp: u16) -> f32 {
		// According to chapter 4.9.5. Temperature Sensor in RP2040 datasheet
		let temp = 27.0 - (raw_temp as f32 * 3.3 / 4096.0 - 0.706) / 0.001721;
		let sign = if temp < 0.0 { -1.0 } else { 1.0 };
		let rounded_temp_x10: i16 = ((temp * 10.0) + 0.5 * sign) as i16;
		(rounded_temp_x10 as f32) / 10.0
	}

	pub fn new(adc: Adc<'a, Async>, ts: Channel<'a>) -> Self {
		Self {
			adc,
			ts
		}
	}

}

impl <'a>TemperatureSensor for BuiltInTemperatureSensor<'a> {
	async fn read_temperature(&mut self) -> f32 {
		let raw_temp = self.adc.read(&mut self.ts).await.unwrap();
		Self::convert_to_celsius(raw_temp)
	}
}

