use super::*;
use serde::Deserialize;

const API_URL: &str = "https://api.weather.gov/";

const MPH_TO_KPH: f64 = 1.609344;

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
struct ApiForecastResponse {
    properties: ApiForecastProperties,
}

#[derive(Deserialize, Debug)]
struct ApiForecastProperties {
    periods: Vec<ApiForecast>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ApiForecast {
    is_daytime: bool,
    temperature: ApiValue,
    relative_humidity: ApiValue,
    wind_speed: ApiValue,
    wind_direction: String,
    short_forecast: String,
}

impl ApiForecast {
    fn wind_direction(&self) -> f64 {
        let dir = match self.wind_direction.as_str() {
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
            _ => 0,
        };
        (dir as f64) * (360.0 / 16.0)
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

    fn wind_kmh(&self) -> f64 {
        if self.wind_speed.unit_code.ends_with("km_h-1") {
            self.wind_speed.value
        } else {
            self.wind_speed.value * MPH_TO_KPH
        }
    }

    fn apparant_temp(&self) -> f64 {
        let temp = if self.temperature.unit_code.ends_with("degC") {
            self.temperature.value
        } else {
            (self.temperature.value - 32.0) * 5.0 / 9.0
        };
        let humidity = self.relative_humidity.value;
        let wind_speed = self.wind_kmh();
        australian_apparent_temp(temp, humidity, wind_speed)
    }

    fn to_moment(&self) -> WeatherMoment {
        let icon = short_forecast_to_icon(&self.short_forecast, !self.is_daytime);
        let weather = Self::icon_to_word(icon);
        WeatherMoment {
            icon: short_forecast_to_icon(&self.short_forecast, !self.is_daytime),
            weather,
            weather_verbose: self.short_forecast.clone(),
            temp: self.temperature.value,
            apparent: self.apparant_temp(),
            humidity: self.relative_humidity.value,
            wind: self.wind_speed.value,
            wind_kmh: self.wind_kmh(),
            wind_direction: Some(self.wind_direction()),
        }
    }

    fn to_aggregate(&self) -> ForecastAggregate {
        ForecastAggregate {
            temp: self.temperature.value,
            apparent: self.apparant_temp(),
            humidity: self.relative_humidity.value,
            wind: self.wind_speed.value,
            wind_kmh: self.wind_kmh(),
            wind_direction: Some(self.wind_direction()),
        }
    }
}

fn aggregate_avg(data: &[ForecastAggregate]) -> ForecastAggregate {
    let mut temp = 0.0;
    let mut apparent = 0.0;
    let mut humidity = 0.0;
    let mut wind_north = 0.0;
    let mut wind_east = 0.0;
    let mut wind_kmh_north = 0.0;
    let mut wind_kmh_east = 0.0;
    for val in data {
        temp += val.temp;
        apparent += val.apparent;
        humidity += val.humidity;
        let (sin, cos) = val
            .wind_direction
            .unwrap_or_default()
            .to_radians()
            .sin_cos();
        wind_north += val.wind * cos;
        wind_east += val.wind * sin;
        wind_kmh_north += val.wind_kmh * cos;
        wind_kmh_east += val.wind_kmh * sin;
    }

    let count = data.len() as f64;
    ForecastAggregate {
        temp,
        apparent,
        humidity,
        wind: wind_east.hypot(wind_north) / count,
        wind_kmh: wind_kmh_east.hypot(wind_kmh_north) / count,
        wind_direction: Some(wind_east.atan2(wind_north).to_degrees().rem_euclid(360.0)),
    }
}

fn aggregate_max(data: &[ForecastAggregate]) -> ForecastAggregate {
    let mut max = ForecastAggregate {
        temp: -1000.0,
        apparent: -1000.0,
        humidity: 0.0,
        wind: 0.0,
        wind_kmh: 0.0,
        wind_direction: Some(0.0),
    };
    for val in data {
        max.temp = max.temp.max(val.temp);
        max.apparent = max.apparent.max(val.apparent);
        max.humidity = max.humidity.max(val.humidity);
        if val.wind > max.wind {
            max.wind_direction = val.wind_direction;
        }
        max.wind = max.wind.max(val.wind);
        max.wind_kmh = max.wind_kmh.max(val.wind_kmh);
    }
    max
}

fn aggregate_min(data: &[ForecastAggregate]) -> ForecastAggregate {
    let mut min = ForecastAggregate {
        temp: 1000.0,
        apparent: 1000.0,
        humidity: 100.0,
        wind: 1000.0,
        wind_kmh: 1000.0,
        wind_direction: Some(0.0),
    };
    for val in data {
        min.temp = min.temp.min(val.temp);
        min.apparent = min.apparent.min(val.apparent);
        min.humidity = min.humidity.min(val.humidity);
        if val.wind > min.wind {
            min.wind_direction = val.wind_direction;
        }
        min.wind = min.wind.min(val.wind);
        min.wind_kmh = min.wind_kmh.min(val.wind_kmh);
    }
    min
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ApiValue {
    value: f64,
    unit_code: String,
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

        let data: ApiForecastResponse = REQWEST_CLIENT
            .get(location.query)
            .header(
                "Feature-Flags",
                "forecast_wind_speed_qv,forecast_temperature_qv",
            )
            .send()
            .await
            .error("weather request failed")?
            .json()
            .await
            .error("parsing weather data failed")?;

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

        let forecast = Some(Forecast {
            avg: aggregate_avg(&data_agg),
            min: aggregate_min(&data_agg),
            max: aggregate_max(&data_agg),
            fin,
        });

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
