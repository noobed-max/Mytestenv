
mod lib;
use libwayshot::{WayshotConnection, region::LogicalRegion};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let wayshot_connection = WayshotConnection::new()?;
    let image_buffer = wayshot_connection.screenshot_all(false)?;
    // Continue with your logic here.
    
    Ok(())
}
