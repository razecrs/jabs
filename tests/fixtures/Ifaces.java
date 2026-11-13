// Interface dispatch: abstract and default methods, most-specific default selection,
// Interface.super calls, static interface methods, interface type tests and arrays.
interface Animal {
    int legs();
    default int eyes() { return 2; }
    default int describe() { return legs() * 10 + eyes(); }
    static int count(Animal a, Animal b) { return a.legs() + b.legs(); }
}

interface Pet extends Animal {
    default int eyes() { return 3; }
}

interface Loud {
    int volume();
}

class Dog implements Pet, Loud {
    public int legs() { return 4; }
    public int volume() { return 9; }
}

class Spider implements Animal {
    public int legs() { return 8; }
    public int eyes() { return 8; }
}

class Puppy implements Pet, Loud {
    public int legs() { return 4; }
    public int volume() { return 3; }
    public int eyes() { return Pet.super.eyes() + 1; }
}

public class Ifaces {
    static Animal make(int which) {
        switch (which) {
            case 0: return new Dog();
            case 1: return new Spider();
            default: return new Puppy();
        }
    }
    static int legs(int w) { return make(w).legs(); }
    static int eyes(int w) { return make(w).eyes(); }
    static int describe(int w) { return make(w).describe(); }
    static int count(int a, int b) { return Animal.count(make(a), make(b)); }
    static int isLoud(int w) { return make(w) instanceof Loud ? 1 : 0; }
    static int castLoud(int w) { return ((Loud) make(w)).volume(); }
    static int dogEyes() { Dog d = new Dog(); return d.eyes(); }
    static int arrayOfAnimals(int n) {
        Animal[] all = new Animal[n];
        for (int i = 0; i < n; i++) all[i] = make(i % 3);
        int total = 0;
        for (Animal a : all) total += a.legs() * 100 + a.eyes();
        return total;
    }
    static int arraysAreCloneable() {
        Object o = new int[3];
        return (o instanceof Cloneable ? 1 : 0) + (o instanceof java.io.Serializable ? 10 : 0) + (o instanceof Loud ? 100 : 0);
    }
    static int nullReceiver() { Animal a = null; return a.legs(); }
}
