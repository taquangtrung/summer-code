//! A greeting, with a test that does not compile against it yet.

pub fn greet() -> String {
    "Hello!".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_greets_by_name() {
        assert_eq!(greet("Ada"), "Hello, Ada!");
        assert_eq!(greet("Grace"), "Hello, Grace!");
    }
}
