pub trait Sample {}

impl Sample for f32 {}
impl Sample for i16 {}

pub trait Samples<T: Sample> {}

impl<T> Samples<T> for [T] where T: Sample {}
impl<T> Samples<T> for Vec<T> where T: Sample {}