use serde::Deserialize;

/// Token counts from the last prompt response.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

/// Context window use and cost from the last `usage_update`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ContextUsage {
    pub used: u64,
    pub size: u64,
    pub cost: Option<Cost>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Cost {
    pub amount: f64,
    pub currency: String,
}
