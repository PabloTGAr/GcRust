GcRust
===============

[![CI](https://github.com/google-apis-rs/google-cloud-rs/actions/workflows/ci.yaml/badge.svg)](https://github.com/google-apis-rs/google-cloud-rs/actions/workflows/ci.yaml)
[![version](https://img.shields.io/crates/v/google-cloud)](https://crates.io/crates/google-cloud)
[![docs](https://docs.rs/google-cloud/badge.svg)](https://docs.rs/google-cloud)
[![license](https://img.shields.io/crates/l/google-cloud)](https://github.com/google-apis-rs/google-cloud-rs#license)

Asynchronous Rust bindings for Google Cloud Platform gRPC APIs.

This library aims to create high-level and idiomatic bindings to Google Cloud Platform APIs and services.

Because of the breadth of the services offered by GCP and the desire to create idiomatic APIs for each of them, it currently only supports a handful of services.  
Contributions for new service integrations are very welcome, since the entirety of GCP can be hard to cover by only a few maintainers.  

If you are looking for lower-level bindings that offer more control and supports a lot more services (through automated code-generation), you can look into using [**`google-apis-rs/generator`**](https://github.com/google-apis-rs/generator).

Implemented services
--------------------

| Service                                               | Feature name | Status          |
| ----------------------------------------------------- | ------------ | --------------- |
| [**Pub/Sub**](https://cloud.google.com/pubsub)        | `pubsub`     | **Complete**    |
| [**Datastore**](https://cloud.google.com/datastore)   | `datastore`  | **Complete**    |
| [**Cloud Storage**](https://cloud.google.com/storage) | `storage`    | **Complete**    |
| [**Cloud Vision**](https://cloud.google.com/vision)   | `vision`     | **In progress** |
| [**Cloud Tasks**](https://cloud.google.com/tasks)     | `tasks`      | **In progress** |

Examples
--------

You can see examples of how to use each of these integrations by looking at their [**different integration tests**](https://github.com/google-apis-rs/google-cloud-rs/tree/master/google-cloud/src/tests), which aims to model how these services are typically used.