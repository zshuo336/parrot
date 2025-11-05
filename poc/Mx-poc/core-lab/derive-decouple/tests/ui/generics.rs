extern crate api_neutral as parrot_api;
use derive_decouple::ParrotActorNeutral;
use std::marker::PhantomData;
#[derive(ParrotActorNeutral)]
struct Generic<'a, T, const N: usize> where T: Send + 'static, 'a: 'static {
    value: T,
    marker: PhantomData<&'a [u8; N]>,
}
impl<'a, T, const N: usize> parrot_api::ActorBehaviour for Generic<'a,T,N>
where T: Send + Clone + 'static, 'a: 'static {
    fn receive(&mut self,_:parrot_api::BoxedMessage)->parrot_api::BoxedResult {Ok(Box::new(self.value.clone()))}
}
fn main(){
    let mut actor=Generic::<'static,u64,8>{value:42,marker:PhantomData};
    let value=parrot_api::ErasedActor::receive(&mut actor,Box::new(())).unwrap();
    assert_eq!(*value.downcast::<u64>().unwrap(),42);
}
