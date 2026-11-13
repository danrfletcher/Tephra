import Foundation

/// A person.
@objc public class Person {
    var name = ""
    let id = 1

    init() {}

    func greet() {}
}

struct Point {
    func norm() -> Int { 0 }
}

enum Mode {
    case fast
    func label() -> String { "" }
}

protocol Shape {
    func area() -> Double
}

extension Person: Shape {
    func area() -> Double { 0 }
}

actor Counter {
    func bump() {}
}

func top(a: Int) -> Int {
    func nested() {}
    let local = 1
    return a
}

let LIMIT = 10
var counter = 0
typealias Name = String
func café() {}
