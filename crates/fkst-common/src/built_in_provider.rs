//! Built-in runtime queue provider contracts.

pub const FAILURE_FACT_SCHEMA: &str = "fkst.failure_fact.v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltInProviderContract {
    pub provider: &'static str,
    pub queue_local_name: &'static str,
    pub producer_label: &'static str,
    pub source_of_truth: &'static str,
    pub payload_schema: &'static str,
    pub trust_boundary: &'static str,
}

impl BuiltInProviderContract {
    pub fn owns_queue(self, queue: &str) -> bool {
        if queue == self.queue_local_name {
            return true;
        }
        if self.queue_local_name.contains('.') {
            return false;
        }
        queue
            .rsplit_once('.')
            .map(|(_, local_name)| local_name == self.queue_local_name)
            .unwrap_or(false)
    }
}

pub const BUILT_IN_DEAD_LETTER_PROVIDER: BuiltInProviderContract = BuiltInProviderContract {
    provider: "runtime.dead_letter",
    queue_local_name: "dead_letter",
    producer_label: "runtime-produced provider 'runtime.dead_letter'",
    source_of_truth: "durable delivery dead table",
    payload_schema: FAILURE_FACT_SCHEMA,
    trust_boundary: "framework reliable delivery runtime",
};

pub const BUILT_IN_FAILURE_FACT_PROVIDER: BuiltInProviderContract = BuiltInProviderContract {
    provider: "runtime.failure_fact",
    queue_local_name: "fkst.failure_fact",
    producer_label: "runtime-produced provider 'runtime.failure_fact'",
    source_of_truth: "framework startup and durable delivery failure paths",
    payload_schema: FAILURE_FACT_SCHEMA,
    trust_boundary: "framework supervise runtime",
};

pub const BUILT_IN_PROVIDER_CONTRACTS: &[BuiltInProviderContract] = &[
    BUILT_IN_DEAD_LETTER_PROVIDER,
    BUILT_IN_FAILURE_FACT_PROVIDER,
];

pub fn built_in_provider_for_queue(queue: &str) -> Option<BuiltInProviderContract> {
    BUILT_IN_PROVIDER_CONTRACTS
        .iter()
        .copied()
        .find(|contract| contract.owns_queue(queue))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_provider_for_queue_covers_dead_letter() {
        let contract = built_in_provider_for_queue("consensus.dead_letter").unwrap();

        assert_eq!(contract.provider, "runtime.dead_letter");
        assert_eq!(contract.queue_local_name, "dead_letter");
        assert_eq!(
            contract.producer_label,
            "runtime-produced provider 'runtime.dead_letter'"
        );
        assert_eq!(contract.source_of_truth, "durable delivery dead table");
        assert_eq!(contract.payload_schema, "fkst.failure_fact.v1");
        assert_eq!(
            contract.trust_boundary,
            "framework reliable delivery runtime"
        );
        assert_eq!(built_in_provider_for_queue("consensus.proposal"), None);
    }

    #[test]
    fn built_in_provider_for_queue_covers_engine_failure_fact() {
        let contract = built_in_provider_for_queue("fkst.failure_fact").unwrap();

        assert_eq!(contract.provider, "runtime.failure_fact");
        assert_eq!(contract.queue_local_name, "fkst.failure_fact");
        assert_eq!(
            contract.producer_label,
            "runtime-produced provider 'runtime.failure_fact'"
        );
        assert_eq!(
            contract.source_of_truth,
            "framework startup and durable delivery failure paths"
        );
        assert_eq!(contract.payload_schema, "fkst.failure_fact.v1");
        assert_eq!(contract.trust_boundary, "framework supervise runtime");
    }
}
