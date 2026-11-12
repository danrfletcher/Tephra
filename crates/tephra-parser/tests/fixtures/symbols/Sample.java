package demo;

/** A service. */
@Deprecated
public class Service implements Runnable {
    private int count = 0;
    static final int LIMIT = 3;

    public Service() {}

    @Override
    public void run() {}

    public void run(int times) {}

    class Inner {
        void go() {}
    }

    enum Level { LOW, HIGH; boolean high() { return true; } }
}

interface Shape {
    double area();
    default String label() { return ""; }
}

enum Direction { NORTH, SOUTH }

record Point(int x, int y) {
    int sum() { return x + y; }
}
