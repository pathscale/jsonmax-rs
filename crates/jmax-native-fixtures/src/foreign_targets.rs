//! Example foreign types constructed only after the native schema accepts.
#[derive(Debug, PartialEq, Eq)]
pub struct Id(pub [u8; 2]);
impl core::str::FromStr for Id {
    type Err = ();
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let bytes: [u8; 2] = value.as_bytes().try_into().map_err(|_| ())?;
        if bytes.iter().all(u8::is_ascii_uppercase) { Ok(Self(bytes)) } else { Err(()) }
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct Label(pub String);
impl TryFrom<&str> for Label {
    type Error = ();
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if value.is_empty() { Err(()) } else { Ok(Self(value.into())) }
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct Quote {
    pub id: Id,
    pub label: Label,
    pub price: i128,
    pub side: i32,
}
pub fn price(value: &jmax_abi::number::Decimal128) -> Option<i128> {
    (value.scale == 8).then_some(value.coefficient)
}
pub fn side(value: &crate::foreign::models::Side) -> Option<i32> {
    Some(match value {
        crate::foreign::models::Side::Bid => -2,
        crate::foreign::models::Side::Ask => 7,
    })
}
