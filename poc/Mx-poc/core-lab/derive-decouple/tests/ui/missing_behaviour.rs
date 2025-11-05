extern crate api_neutral as parrot_api;
use derive_decouple::ParrotActorNeutral;
#[derive(ParrotActorNeutral)]
struct Missing;
fn main(){}
