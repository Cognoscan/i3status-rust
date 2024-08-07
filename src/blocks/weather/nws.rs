//! Support for using the US National Weather Service API.
//!
//! The API is documented [here](https://www.weather.gov/documentation/services-web-api).
//! There is a corresponding [OpenAPI document](https://api.weather.gov/openapi.json). The forecast
//! descriptions are translated into the set of supported icons as best as possible, and a more
//! complete summary forecast is available in the `weather_verbose` format key. The full NWS list
//! of icons and corresponding descriptions can be found [here](https://api.weather.gov/icons),
//! though these are slated for deprecation.
//!
//! All data is gathered using the hourly weather forecast service, after resolving from latitude &
//! longitude coordinates to a specific forecast office and grid point.
//!

use super::*;
use serde::Deserialize;

const API_URL: &str = "https://api.weather.gov/";

const MPH_TO_KMH: f64 = 1.609344;
const MPH_TO_MS: f64 = 1.609344 / 3.6;
const KMH_TO_MS: f64 = 1.0 / 3.6;

#[derive(Deserialize, Debug, SmartDefault)]
#[serde(tag = "name", rename_all = "lowercase", deny_unknown_fields, default)]
pub struct Config {
    coordinates: Option<(String, String)>,
    #[default(12)]
    forecast_hours: usize,
    #[serde(default)]
    units: UnitSystem,
}

#[derive(Clone, Debug)]
struct LocationInfo {
    query: String,
    name: String,
}

pub(super) struct Service<'a> {
    config: &'a Config,
    location: Option<LocationInfo>,
}

impl<'a> Service<'a> {
    pub(super) async fn new(autolocate: bool, config: &'a Config) -> Result<Service<'a>> {
        let location = if autolocate {
            None
        } else {
            let coords = config.coordinates.as_ref().error("no location given")?;
            Some(Self::get_location_query(&coords.0, &coords.1, config.units).await?)
        };
        Ok(Self { config, location })
    }

    async fn get_location_query(lat: &str, lon: &str, units: UnitSystem) -> Result<LocationInfo> {
        let points_url = format!("{API_URL}/points/{lat},{lon}");

        let response: ApiPoints = REQWEST_CLIENT
            .get(points_url)
            .send()
            .await
            .error("Zone resolution request failed")?
            .json()
            .await
            .error("Failed to parse zone resolution request")?;
        let mut query = response.properties.forecast_hourly;
        query.push_str(match units {
            UnitSystem::Metric => "?units=si",
            UnitSystem::Imperial => "?units=us",
        });
        let location = response.properties.relative_location.properties;
        let name = format!("{}, {}", location.city, location.state);
        Ok(LocationInfo { query, name })
    }
}

#[derive(Deserialize, Debug)]
struct ApiPoints {
    properties: ApiPointsProperties,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ApiPointsProperties {
    forecast_hourly: String,
    relative_location: ApiRelativeLocation,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ApiRelativeLocation {
    properties: ApiRelativeLocationProperties,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ApiRelativeLocationProperties {
    city: String,
    state: String,
}

#[derive(Deserialize, Debug)]
struct ApiForecastResponse<'a> {
    #[serde(borrow)]
    properties: ApiForecastProperties<'a>,
}

#[derive(Deserialize, Debug)]
struct ApiForecastProperties<'a> {
    #[serde(borrow)]
    periods: Vec<ApiForecast<'a>>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ApiValue<'a> {
    value: f64,
    unit_code: &'a str,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ApiForecast<'a> {
    is_daytime: bool,
    temperature: ApiValue<'a>,
    relative_humidity: ApiValue<'a>,
    wind_speed: ApiValue<'a>,
    wind_direction: &'a str,
    short_forecast: &'a str,
}

impl<'a> ApiForecast<'a> {
    fn wind_direction(&self) -> Option<f64> {
        let dir = match self.wind_direction {
            "N" => 0,
            "NNE" => 1,
            "NE" => 2,
            "ENE" => 3,
            "E" => 4,
            "ESE" => 5,
            "SE" => 6,
            "SSE" => 7,
            "S" => 8,
            "SSW" => 9,
            "SW" => 10,
            "WSW" => 11,
            "W" => 12,
            "WNW" => 13,
            "NW" => 14,
            "NNW" => 15,
            _ => return None,
        };
        Some((dir as f64) * (360.0 / 16.0))
    }

    fn icon_to_word(icon: WeatherIcon) -> String {
        match icon {
            WeatherIcon::Clear { .. } => "Clear",
            WeatherIcon::Clouds { .. } => "Clouds",
            WeatherIcon::Fog { .. } => "Fog",
            WeatherIcon::Thunder { .. } => "Thunder",
            WeatherIcon::Rain { .. } => "Rain",
            WeatherIcon::Snow => "Snow",
            WeatherIcon::Default => "Unknown",
        }
        .to_string()
    }

    fn wind_speed_kmh(&self) -> f64 {
        if self.wind_speed.unit_code.ends_with("km_h-1") {
            self.wind_speed.value
        } else {
            self.wind_speed.value * MPH_TO_KMH
        }
    }

    fn wind_speed_local_units(&self) -> f64 {
        if self.wind_speed.unit_code.ends_with("km_h-1") {
            self.wind_speed.value * KMH_TO_MS
        } else {
            self.wind_speed.value
        }
    }

    fn wind_speed_ms(&self) -> f64 {
        if self.wind_speed.unit_code.ends_with("km_h-1") {
            self.wind_speed.value * KMH_TO_MS
        } else {
            self.wind_speed.value * MPH_TO_MS
        }
    }

    fn apparent_temp(&self) -> f64 {
        let temp = if self.temperature.unit_code.ends_with("degC") {
            self.temperature.value
        } else {
            (self.temperature.value - 32.0) * 5.0 / 9.0
        };
        let humidity = self.relative_humidity.value;
        let wind_speed = self.wind_speed_ms();
        let apparent = australian_apparent_temp(temp, humidity, wind_speed);
        if self.temperature.unit_code.ends_with("degC") {
            apparent
        } else {
            apparent * 9.0 / 5.0 + 32.0
        }
    }

    fn to_moment(&self) -> WeatherMoment {
        let icon = short_forecast_to_icon(self.short_forecast, !self.is_daytime);
        let weather = Self::icon_to_word(icon);
        WeatherMoment {
            icon,
            weather,
            weather_verbose: self.short_forecast.to_string(),
            temp: self.temperature.value,
            apparent: self.apparent_temp(),
            humidity: self.relative_humidity.value,
            wind: self.wind_speed_local_units(),
            wind_kmh: self.wind_speed_kmh(),
            wind_direction: self.wind_direction(),
        }
    }

    fn to_aggregate(&self) -> ForecastAggregate {
        ForecastAggregate {
            temp: self.temperature.value,
            apparent: self.apparent_temp(),
            humidity: self.relative_humidity.value,
            wind: self.wind_speed_local_units(),
            wind_kmh: self.wind_speed_kmh(),
            wind_direction: self.wind_direction(),
        }
    }
}

fn combine_forecasts(data: &[ForecastAggregate], fin: WeatherMoment) -> Forecast {
    let mut temp = 0.0;
    let mut apparent = 0.0;
    let mut humidity = 0.0;
    let mut wind_north = 0.0;
    let mut wind_east = 0.0;
    let mut wind_kmh_north = 0.0;
    let mut wind_kmh_east = 0.0;
    let mut wind_count = 0.0;
    let mut max = ForecastAggregate {
        temp: f64::MIN,
        apparent: f64::MIN,
        humidity: f64::MIN,
        wind: f64::MIN,
        wind_kmh: f64::MIN,
        wind_direction: None,
    };
    let mut min = ForecastAggregate {
        temp: f64::MAX,
        apparent: f64::MAX,
        humidity: f64::MAX,
        wind: f64::MAX,
        wind_kmh: f64::MAX,
        wind_direction: None,
    };
    for val in data {
        // Summations for averaging
        temp += val.temp;
        apparent += val.apparent;
        humidity += val.humidity;
        if let Some(degrees) = val.wind_direction {
            let (sin, cos) = degrees.to_radians().sin_cos();
            wind_north += val.wind * cos;
            wind_east += val.wind * sin;
            wind_kmh_north += val.wind_kmh * cos;
            wind_kmh_east += val.wind_kmh * sin;
            wind_count += 1.0;
        }

        // Max
        max.temp = max.temp.max(val.temp);
        max.apparent = max.apparent.max(val.apparent);
        max.humidity = max.humidity.max(val.humidity);
        if val.wind > max.wind {
            max.wind_direction = val.wind_direction;
            max.wind = val.wind;
            max.wind_kmh = val.wind_kmh;
        }

        // Min
        min.temp = min.temp.min(val.temp);
        min.apparent = min.apparent.min(val.apparent);
        min.humidity = min.humidity.min(val.humidity);
        if val.wind < min.wind {
            min.wind_direction = val.wind_direction;
            min.wind = val.wind;
            min.wind_kmh = val.wind_kmh;
        }
    }

    let count = data.len() as f64;

    // Calculate the wind results separately, discarding invalid wind values
    let (wind, wind_kmh, wind_direction) = if wind_count == 0.0 {
        (0.0, 0.0, None)
    } else {
        (
            wind_east.hypot(wind_north) / wind_count,
            wind_kmh_east.hypot(wind_kmh_north) / wind_count,
            Some(wind_east.atan2(wind_north).to_degrees().rem_euclid(360.0)),
        )
    };

    let avg = ForecastAggregate {
        temp: temp / count,
        apparent: apparent / count,
        humidity: humidity / count,
        wind,
        wind_kmh,
        wind_direction,
    };
    Forecast { avg, min, max, fin }
}

#[async_trait]
impl WeatherProvider for Service<'_> {
    async fn get_weather(
        &self,
        autolocated: Option<&Coordinates>,
        need_forecast: bool,
    ) -> Result<WeatherResult> {
        let location = if let Some(coords) = autolocated {
            Self::get_location_query(
                &coords.latitude.to_string(),
                &coords.longitude.to_string(),
                self.config.units,
            )
            .await?
        } else {
            self.location.clone().error("No location was provided")?
        };

        let resp = REQWEST_CLIENT
            .get(location.query)
            .header(
                "Feature-Flags",
                "forecast_wind_speed_qv,forecast_temperature_qv",
            )
            .send()
            .await
            .error("weather request failed")?
            .bytes()
            .await
            .error("retrieving weather data failed")?;
        let data: ApiForecastResponse =
            serde_json::from_slice(&resp).error("parsing weather data failed")?;

        let data = data.properties.periods;
        let current = data.first().error("No current weather")?;

        let current_weather = current.to_moment();

        if !need_forecast {
            return Ok(WeatherResult {
                location: location.name,
                current_weather,
                forecast: None,
            });
        }

        let data_agg: Vec<ForecastAggregate> = data
            .iter()
            .take(self.config.forecast_hours)
            .map(|f| f.to_aggregate())
            .collect();

        let fin = data
            .get(self.config.forecast_hours.min(data.len() - 1))
            .error("no weather available")?
            .to_moment();

        let forecast = Some(combine_forecasts(&data_agg, fin));

        Ok(WeatherResult {
            location: location.name,
            current_weather,
            forecast,
        })
    }
}

/// Try to turn the short forecast into an icon.
///
/// The official API has an icon field, but it's been marked as deprecated.
/// Unfortunately, the short forecast cannot actually be fully enumerated, so
/// we're reduced to checking for the presence of specific strings.
fn short_forecast_to_icon(weather: &str, is_night: bool) -> WeatherIcon {
    let weather = weather.to_lowercase();
    // snow, flurries, flurry, blizzard
    if weather.contains("snow") || weather.contains("flurr") || weather.contains("blizzard") {
        return WeatherIcon::Snow;
    }
    // thunderstorms
    if weather.contains("thunder") {
        return WeatherIcon::Thunder { is_night };
    }
    // fog or mist
    if weather.contains("fog") || weather.contains("mist") {
        return WeatherIcon::Fog { is_night };
    }
    // rain, rainy, shower, drizzle (drizzle might not be present)
    if weather.contains("rain") || weather.contains("shower") || weather.contains("drizzle") {
        return WeatherIcon::Rain { is_night };
    }
    // cloudy, clouds, partly cloudy, overcast, etc.
    if weather.contains("cloud") || weather.contains("overcast") {
        return WeatherIcon::Clouds { is_night };
    }
    // clear (night), sunny (day). "Mostly sunny" / "Mostly clear" fit here too
    if weather.contains("clear") || weather.contains("sunny") {
        return WeatherIcon::Clear { is_night };
    }
    WeatherIcon::Default
}
