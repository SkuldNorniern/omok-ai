mod plot;
mod trainer;
mod utils;

use tensorflow::Status;
use trainer::Trainer;

fn main() -> Result<(), Status> {
    let mut train = Trainer::new()?;
    train.train(10_000)?;
    Ok(())
}
