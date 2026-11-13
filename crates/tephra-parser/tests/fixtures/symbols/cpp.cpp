#include <string>

namespace app {

int version = 1;

class Widget : public Base {
public:
    Widget();
    ~Widget();
    int size() const;
    void draw() {}
private:
    int w;
};

struct Config {
    void reset() {}
};

enum class Mode { Fast, Slow };

using Name = std::string;

}  // namespace app

int Widget::size() const { return 1; }

int over(int a) { return a; }
int over(double a) { return 1; }

template <typename T>
T identity(T value) { return value; }
