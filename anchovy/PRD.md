we are building a ground-up rewrite of the sui validator and fullnode. because this is a very large project, we will need to proceed carefully, in small phases

Top level rules / design principles:
- start the work in a new workspace
- do not take any dependencies on existing crates in the sui repo, (or in the MystenLabs org generally) until/unless explicitly instructed
  - exceptions:
    - fastcrypto, we will not re-implement any crypto routines
    - tidehunter
    - bcs
- 3rd party community crates are okay to use but:
  - ubiquitous infrastructure things like tokio, tonic, etc are okay.
  - be cautious about taking anything with a ton of dependencies
- performance and correctness are the number one concerns:
  - the main performance tools we have are:
    - minimize allocations / frees.
    - minimize indirection / pointer chasing
    - prefer processing memory sequentially whenever possible
  - correctness tools:
    - use types to "force" correctness when possible
      - an example of this is the use of VerifiedTransaction in the existing sui implementation, which prevents an unverified transaction from being executed.
    - write "logically sequential" code. that is, prefer organizing code into functions that do steps 1 through N in order, such that the function reads like a spec. avoid excessive abstraction and indirection.
    - prefer modest amounts of code duplication over prematurely abstracting. for instance, if the validator and fullnode do things slightly differently, its okay to have one function for each role. in such cases, add a comment that reminds future agents to update the fullnode function when modifying the validator function, and vice versa.
    - liberally pull in test cases from the existing impl to validate the new impl.
    - use the old ipml as a "black box" reference implementation when its helpful. (although note that bugs may exist in the reference impl) 


Phase 1: re-implement wire format types from sui-types.
- this only includes the types that are enumerated by generate_format.rs and listed in format__sui.yaml.snap
- We need a parser that minimizes copies. It should take the buffer red off the wire and deserialize into a structure that can hold references back to the buffer. This way we don't have to copy unnecessarily. We also want all allocations for a deserialized message to be in a single buffer as well (use a custom allocator). when the message is no longer needed we must be able to drop it with two free() calls (one for the original message buf, one for the deserialization arena)
- there are many higher-level things we read from parsed transactions, For instance, the set of shared input objects. In the existing implementation this is usually done by walking over the transaction and adding things to a vector. We want to pre-populate all of this data at parse time, so that it can always be returned as a simple iterator over an existing buffer.
- we will need separate "builder" structs to build messages, since the primary type will only be constructible from an already serialized bcs buffer.
- build a fuzzer to test the deser. look for potential OOMs
- benchmark the deser path against real transactions pulled from mainnet, and optimize it.

---

Clarifications (added after the original prompt, in the user's words):

- (Phase 1, on the checks sui's Deserialize impls run beyond the wire format: signature parsing, BLS point and roaring bitmap checks, identifier grammar, party permissions, etc.) "the above items must be deferred until Validation time. This is because we may load transactions from trusted sources such as the database, In which case we do not want to redo this validation. The only goal of deserialization should be to create an in-memory representation."
