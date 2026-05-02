use async_trait::async_trait;

pub struct GetWeatherTool;

#[async_trait]
impl metalcraft::Tool for GetWeatherTool {
    fn name(&self) -> &str { "get_weather" }
    fn description(&self) -> &str {
        "Get the current weather for a given city. Returns temperature, conditions, and humidity."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "city": {
                    "type": "string",
                    "description": "The city name to get weather for"
                }
            },
            "required": ["city"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let city = args["city"].as_str().unwrap_or("Unknown");

        let (temp, conditions, humidity) = match city.to_lowercase().as_str() {
            "chicago" => (45, "Windy and partly cloudy", 62),
            "new york" | "nyc" => (52, "Overcast", 58),
            "los angeles" | "la" => (72, "Sunny", 35),
            "miami" => (82, "Hot and humid", 78),
            "seattle" => (48, "Rainy", 85),
            "denver" => (55, "Clear skies", 30),
            _ => (65, "Fair", 50),
        };

        Ok(serde_json::json!({
            "city": city,
            "temperature_f": temp,
            "conditions": conditions,
            "humidity_pct": humidity,
        }))
    }
}
