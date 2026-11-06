//! Provider-neutral conversation types.

// Rust note: `use` brings a name into scope so we can write `fmt` instead of
// `std::fmt`. `std` is Rust's standard library.
use std::fmt;

/// One message in the conversation. History is a plain `Vec<Message>` that only grows.
//
// Rust note: `#[derive(...)]` asks the compiler to write standard trait
// implementations for us:
//   Debug     - lets us print the value with `{:?}` (used in test failures)
//   Clone     - gives a `.clone()` method that makes an independent copy
//   PartialEq - lets us compare two values with `==`
//   Eq        - promises that `==` is a true equality (needed by some APIs)
// A "trait" is Rust's word for a set of behaviours a type can have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    // Rust note: an `enum` is a value that is exactly ONE of several variants,
    // and each variant can carry its own data. A `Message` is a System message
    // OR a User message OR an Assistant message OR a Tool message, never two.
    //
    // `String` is an owned, growable piece of text. (`&str`, which you'll see in
    // assemble.rs, is a borrowed view of text someone else owns.)
    System(String),
    User(String),
    Assistant {
        // Rust note: Rust has no `null`. `Option<T>` is either `Some(value)` or
        // `None`. Here the assistant may reply with only a tool call and no text.
        content: Option<String>,
        // Rust note: `Vec<T>` is a growable list. It may be empty.
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

/// A tool call requested by the model.
///
/// `arguments` is the raw JSON text exactly as the model sent it. It is never
/// parsed and re-serialized, so whitespace and key order survive untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
// Rust note: a `struct` groups named fields. `pub` before a field makes it
// readable and writable from other crates. Fields without `pub` are private.
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// A tool the model may call.
///
/// `parameters` is a JSON Schema kept as the text its author wrote, so key order
/// is whatever they chose. It is validated once at construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    // Rust note: this field is deliberately NOT `pub`. Outside this module nobody
    // can write `ToolSpec { ... }` directly, because they could not fill in a
    // private field. The only way to get a `ToolSpec` is `ToolSpec::new`, which
    // checks the JSON. So every `ToolSpec` that exists is known to be valid.
    parameters: String,
}

// Rust note: an `impl` block attaches functions to a type.
impl ToolSpec {
    /// Builds a spec, rejecting `parameters` that is not valid JSON.
    //
    // Rust note: `impl Into<String>` means "any value that can be turned into a
    // String", so callers may pass a `&str` literal or an existing `String`.
    // We call `.into()` below to do the conversion.
    //
    // The return type `Result<Self, InvalidParameters>` is Rust's way of
    // returning "success or failure": `Ok(value)` or `Err(error)`. `Self` just
    // means `ToolSpec` here.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: impl Into<String>,
    ) -> Result<Self, InvalidParameters> {
        let parameters = parameters.into();
        // Rust note, step by step:
        // - `from_str::<&RawValue>` parses the text only to check it is valid
        //   JSON. The `::<...>` part tells the function what type to produce
        //   (people call it "turbofish").
        // - `&parameters` borrows the text instead of moving it, so we can keep
        //   using `parameters` afterwards.
        // - `.map_err(InvalidParameters)` converts the library's error into our
        //   own error type. A tuple struct's name works as a function.
        // - `?` means: if this is an `Err`, return it from `new` right now;
        //   otherwise carry on. The parsed value itself is thrown away.
        serde_json::from_str::<&serde_json::value::RawValue>(&parameters)
            .map_err(InvalidParameters)?;
        // Rust note: the last expression of a function, with no `;`, is what the
        // function returns. `parameters,` is shorthand for `parameters: parameters,`.
        Ok(Self {
            name: name.into(),
            description: description.into(),
            parameters,
        })
    }

    /// The schema text exactly as given to [`ToolSpec::new`].
    //
    // Rust note: `&self` means "borrow this ToolSpec, read-only". Returning
    // `&str` hands out a borrowed view of the stored text, with no copy. The
    // compiler guarantees the view can't outlive the `ToolSpec`.
    pub fn parameters(&self) -> &str {
        &self.parameters
    }
}

/// The tool schema text was not valid JSON.
#[derive(Debug)]
// Rust note: a "tuple struct": a struct whose fields have no names. The single
// field is reached as `.0`. It is private, so callers see only the type.
pub struct InvalidParameters(serde_json::Error);

// Rust note: implementing the `Display` trait is what lets a value be printed
// with `{}` in `println!`/`format!`. `write!` writes formatted text into `f`.
impl fmt::Display for InvalidParameters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tool parameters are not valid JSON: {}", self.0)
    }
}

// Rust note: an empty `impl` is enough to mark this type as a standard error
// (it needs `Debug` and `Display`, which it has). That lets it work with `?`
// and `Box<dyn Error>` in other code.
impl std::error::Error for InvalidParameters {}

/// Router-level options, all optional. A typed struct on purpose: a free-form
/// map would let key order leak into the request bytes.
//
// Rust note: `Default` generates `RouterOptions::default()`, a value where every
// field is its own default: empty `Vec`, `None`, empty `String`, `0`, `false`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouterOptions {
    /// OpenRouter `provider.order`: providers to try, in this order.
    pub provider_order: Vec<String>,
    /// OpenRouter `provider.allow_fallbacks`.
    pub allow_fallbacks: Option<bool>,
    /// Stable session key so a router can keep routing to the same provider.
    pub session_id: Option<String>,
}

/// Everything about a request except the messages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub model: String,
    pub tools: Vec<ToolSpec>,
    // Rust note: `u32` is an unsigned 32-bit integer (0 up to about 4 billion).
    pub max_tokens: Option<u32>,
    pub router: RouterOptions,
}
