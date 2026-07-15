//! A catalog of unit conversions, one function per unit.
//!
//! Every function answers the same question in its own unit: how many base
//! units is `n` of this one? The base of each family is named in the
//! function's doc line. `BASES` lists the families; `COUNT` is how many
//! conversions this file defines.

/// The base unit of each family this catalog covers.
pub const BASES: &[&str] = &["metre", "gram", "litre", "second"];

/// How many conversion functions this file defines.
pub const COUNT: usize = 28;

/// `n` millimetres in metres.
pub fn millimetre_to_metre(n: f64) -> f64 {
    n * 0.001
}

/// `n` centimetres in metres.
pub fn centimetre_to_metre(n: f64) -> f64 {
    n * 0.01
}

/// `n` kilometres in metres.
pub fn kilometre_to_metre(n: f64) -> f64 {
    n * 1000.0
}

/// `n` inchs in metres.
pub fn inch_to_metre(n: f64) -> f64 {
    n * 0.0254
}

/// `n` foots in metres.
pub fn foot_to_metre(n: f64) -> f64 {
    n * 0.3048
}

/// `n` yards in metres.
pub fn yard_to_metre(n: f64) -> f64 {
    n * 0.9144
}

/// `n` miles in metres.
pub fn mile_to_metre(n: f64) -> f64 {
    n * 1609.344
}

/// `n` nautical_miles in metres.
pub fn nautical_mile_to_metre(n: f64) -> f64 {
    n * 1852.0
}

/// `n` furlongs in metres.
pub fn furlong_to_metre(n: f64) -> f64 {
    n * 201.168
}

/// `n` milligrams in grams.
pub fn milligram_to_gram(n: f64) -> f64 {
    n * 0.001
}

/// `n` kilograms in grams.
pub fn kilogram_to_gram(n: f64) -> f64 {
    n * 1000.0
}

/// `n` tonnes in grams.
pub fn tonne_to_gram(n: f64) -> f64 {
    n * 1000000.0
}

/// `n` ounces in grams.
pub fn ounce_to_gram(n: f64) -> f64 {
    n * 28.349523125
}

/// `n` pounds in grams.
pub fn pound_to_gram(n: f64) -> f64 {
    n * 453.59237
}

/// `n` stones in grams.
pub fn stone_to_gram(n: f64) -> f64 {
    n * 6350.29318
}

/// `n` millilitres in litres.
pub fn millilitre_to_litre(n: f64) -> f64 {
    n * 0.001
}

/// `n` centilitres in litres.
pub fn centilitre_to_litre(n: f64) -> f64 {
    n * 0.01
}

/// `n` hectolitres in litres.
pub fn hectolitre_to_litre(n: f64) -> f64 {
    n * 100.0
}

/// `n` pint_uks in litres.
pub fn pint_uk_to_litre(n: f64) -> f64 {
    n * 0.56826125
}

/// `n` quart_uks in litres.
pub fn quart_uk_to_litre(n: f64) -> f64 {
    n * 1.1365225
}

/// `n` gallon_uks in litres.
pub fn gallon_uk_to_litre(n: f64) -> f64 {
    n * 4.54609
}

/// `n` fluid_ounce_uks in litres.
pub fn fluid_ounce_uk_to_litre(n: f64) -> f64 {
    n * 0.0284130625
}

/// `n` barrel_oils in litres.
pub fn barrel_oil_to_litre(n: f64) -> f64 {
    n * 158.987294928
}

/// `n` minutes in seconds.
pub fn minute_to_second(n: f64) -> f64 {
    n * 60.0
}

/// `n` hours in seconds.
pub fn hour_to_second(n: f64) -> f64 {
    n * 3600.0
}

/// `n` days in seconds.
pub fn day_to_second(n: f64) -> f64 {
    n * 86400.0
}

/// `n` weeks in seconds.
pub fn week_to_second(n: f64) -> f64 {
    n * 604800.0
}

/// `n` fortnights in seconds.
pub fn fortnight_to_second(n: f64) -> f64 {
    n * 1209600.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mile_is_a_known_number_of_metres() {
        assert_eq!(mile_to_metre(1.0), 1609.344);
    }
}
