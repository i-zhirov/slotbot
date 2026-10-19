/// One concrete available date+time at an office for a service.
///
/// Shared between providers: Invitro fills `time` with the exact hour
/// (e.g. `10:00`); Harizma only exposes the next appointment date, so its
/// slots have an empty `time`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AvailableSlot {
    pub office_id: String,
    pub office_name: String,
    pub service_id: String,
    pub service_name: String,
    pub price: String,
    pub date: String,
    pub time: String,
}
