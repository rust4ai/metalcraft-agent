pub mod get_weather;
pub mod report_result;

use metalcraft::ToolRegistry;

pub fn create_registry() -> ToolRegistry {
    ToolRegistry::new()
        .register(get_weather::GetWeatherTool)
        .register(report_result::ReportResultTool)
}
