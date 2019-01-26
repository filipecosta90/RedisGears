#include "utils/arr_rm_alloc.h"
#include "record.h"

#include "redisgears.h"
#include "redisgears_memory.h"
#ifdef WITHPYTHON
#include "redisgears_python.h"
#endif

#include <pthread.h>

typedef Record* (*Record_Alloc)();
typedef void (*Record_Dispose)(Record* r);
typedef void (*Record_Free)(Record* r);

pthread_key_t _recordAllocatorKey;
pthread_key_t _recordDisposeKey;
pthread_key_t _recordFreeMemoryKey;


Record StopRecord = {
        .type = STOP_RECORD,
};

static inline Record* RG_DefaultAllocator(){
    return RG_ALLOC(sizeof(Record));
}

void RG_SetRecordAlocator(enum RecordAllocator allocator){
    switch(allocator){
    case DEFAULT:
        pthread_setspecific(_recordAllocatorKey, RG_DefaultAllocator);
        pthread_setspecific(_recordDisposeKey, RG_DisposeRecord);
        pthread_setspecific(_recordFreeMemoryKey, RG_FREE);
        break;
    case PYTHON:
        pthread_setspecific(_recordAllocatorKey, RedisGearsPy_AllocatePyRecord);
        pthread_setspecific(_recordDisposeKey, RedisGearsPy_DisposePyRecord);
        pthread_setspecific(_recordFreeMemoryKey, NULL); // python handles his own memory!!
        break;
    default:
        assert(false);
    }
}

int RG_RecordInit(){
    int err = pthread_key_create(&_recordAllocatorKey, NULL);
    err &= pthread_key_create(&_recordDisposeKey, NULL);
    err &= pthread_key_create(&_recordFreeMemoryKey, NULL);
    RG_SetRecordAlocator(DEFAULT);
    return !err;
}

static inline Record* RecordAlloc(){
    Record_Alloc alloc = pthread_getspecific(_recordAllocatorKey);
    return alloc();
}

static inline void RecordDispose(Record* r){
    Record_Dispose dispose = pthread_getspecific(_recordDisposeKey);
    dispose(r);
}

static inline void RecordFree(Record* r){
    Record_Free free = pthread_getspecific(_recordFreeMemoryKey);
    if(free){
        free(r);
    }
}

void RG_DisposeRecord(Record* record){
    dictIterator *iter;
    dictEntry *entry;
    Record* temp;
    switch(record->type){
    case STRING_RECORD:
        RG_FREE(record->stringRecord.str);
        break;
    case LONG_RECORD:
    case DOUBLE_RECORD:
        break;
    case LIST_RECORD:
        for(size_t i = 0 ; i < RedisGears_ListRecordLen(record) ; ++i){
            RG_FreeRecord(record->listRecord.records[i]);
        }
        array_free(record->listRecord.records);
        break;
    case KEY_RECORD:
        if(record->keyRecord.key){
            RG_FREE(record->keyRecord.key);
        }
        if(record->keyRecord.record){
            RG_FreeRecord(record->keyRecord.record);
        }
        break;
    case KEY_HANDLER_RECORD:
        RedisModule_CloseKey(record->keyHandlerRecord.keyHandler);
        break;
    case HASH_SET_RECORD:
        iter = dictGetIterator(record->hashSetRecord.d);
        entry = NULL;
        while((entry = dictNext(iter))){
            temp = dictGetVal(entry);
            RG_FreeRecord(temp);
        }
        dictReleaseIterator(iter);
        dictRelease(record->hashSetRecord.d);
        break;
    default:
        assert(false);
    }
    RecordFree(record);
}

void RG_FreeRecord(Record* record){
#ifdef WITHPYTHON
    if(record->type == PY_RECORD){
        if(record->pyRecord.obj){
            PyGILState_STATE state = PyGILState_Ensure();
            Py_DECREF(record->pyRecord.obj);
            PyGILState_Release(state);
        }
        RG_FREE(record);
        return;
    }
#endif
    RecordDispose(record);
}

enum RecordType RG_RecordGetType(Record* r){
    return r->type;
}

Record* RG_KeyRecordCreate(){
    Record* ret = RecordAlloc();
    ret->type = KEY_RECORD;
    ret->keyRecord.key = NULL;
    ret->keyRecord.len = 0;
    ret->keyRecord.record = NULL;
    return ret;
}

void RG_KeyRecordSetKey(Record* r, char* key, size_t len){
    assert(r->type == KEY_RECORD);
    r->keyRecord.key = key;
    r->keyRecord.len = len;
}
void RG_KeyRecordSetVal(Record* r, Record* val){
    assert(r->type == KEY_RECORD);
    r->keyRecord.record = val;
}

Record* RG_KeyRecordGetVal(Record* r){
    assert(r->type == KEY_RECORD);
    return r->keyRecord.record;
}
char* RG_KeyRecordGetKey(Record* r, size_t* len){
    assert(r->type == KEY_RECORD);
    if(len){
        *len = r->keyRecord.len;
    }
    return r->keyRecord.key;
}
Record* RG_ListRecordCreate(size_t initSize){
    Record* ret = RecordAlloc();
    ret->type = LIST_RECORD;
    ret->listRecord.records = array_new(Record*, initSize);
    return ret;
}

size_t RG_ListRecordLen(Record* r){
    assert(r->type == LIST_RECORD);
    return array_len(r->listRecord.records);
}

void RG_ListRecordAdd(Record* r, Record* element){
    assert(r->type == LIST_RECORD);
    r->listRecord.records = array_append(r->listRecord.records, element);
}

Record* RG_ListRecordGet(Record* r, size_t index){
    assert(r->type == LIST_RECORD);
    assert(RG_ListRecordLen(r) > index && index >= 0);
    return r->listRecord.records[index];
}

Record* RG_ListRecordPop(Record* r){
    return array_pop(r->listRecord.records);
}

Record* RG_StringRecordCreate(char* val, size_t len){
    Record* ret = RecordAlloc();
    ret->type = STRING_RECORD;
    ret->stringRecord.str = val;
    ret->stringRecord.len = len;
    return ret;
}

char* RG_StringRecordGet(Record* r, size_t* len){
    assert(r->type == STRING_RECORD);
    if(len){
        *len = r->stringRecord.len;
    }
    return r->stringRecord.str;
}

void RG_StringRecordSet(Record* r, char* val, size_t len){
    assert(r->type == STRING_RECORD);
    r->stringRecord.str = val;
    r->stringRecord.len = len;
}

Record* RG_DoubleRecordCreate(double val){
    Record* ret = RecordAlloc();
    ret->type = DOUBLE_RECORD;
    ret->doubleRecord.num = val;
    return ret;
}

double RG_DoubleRecordGet(Record* r){
    assert(r->type == DOUBLE_RECORD);
    return r->doubleRecord.num;
}

void RG_DoubleRecordSet(Record* r, double val){
    assert(r->type == DOUBLE_RECORD);
    r->doubleRecord.num = val;
}

Record* RG_LongRecordCreate(long val){
    Record* ret = RecordAlloc();
    ret->type = LONG_RECORD;
    ret->longRecord.num = val;
    return ret;
}
long RG_LongRecordGet(Record* r){
    assert(r->type == LONG_RECORD);
    return r->longRecord.num;
}
void RG_LongRecordSet(Record* r, long val){
    assert(r->type == LONG_RECORD);
    r->longRecord.num = val;
}

Record* RG_HashSetRecordCreate(){
    Record* ret = RecordAlloc();
    ret->type = HASH_SET_RECORD;
    ret->hashSetRecord.d = dictCreate(&dictTypeHeapStrings, NULL);
    return ret;
}

int RG_HashSetRecordSet(Record* r, char* key, Record* val){
    assert(r->type == HASH_SET_RECORD);
    Record* oldVal = RG_HashSetRecordGet(r, key);
    if(oldVal){
        RG_FreeRecord(oldVal);
        dictDelete(r->hashSetRecord.d, key);
    }
    return dictAdd(r->hashSetRecord.d, key, val) == DICT_OK;
}

Record* RG_HashSetRecordGet(Record* r, char* key){
    assert(r->type == HASH_SET_RECORD);
    dictEntry *entry = dictFind(r->hashSetRecord.d, key);
    if(!entry){
        return 0;
    }
    return dictGetVal(entry);
}

char** RG_HashSetRecordGetAllKeys(Record* r, size_t* len){
    assert(r->type == HASH_SET_RECORD);
    dictIterator *iter = dictGetIterator(r->hashSetRecord.d);
    dictEntry *entry = NULL;
    char** ret = array_new(char*, dictSize(r->hashSetRecord.d));
    while((entry = dictNext(iter))){
        char* key = dictGetKey(entry);
        ret = array_append(ret, key);
    }
    *len = array_len(ret);
    dictReleaseIterator(iter);
    return ret;
}

void RG_HashSetRecordFreeKeysArray(char** keyArr){
    array_free(keyArr);
}

Record* RG_KeyHandlerRecordCreate(RedisModuleKey* handler){
    Record* ret = RecordAlloc();
    ret->type = KEY_HANDLER_RECORD;
    ret->keyHandlerRecord.keyHandler = handler;
    return ret;
}

RedisModuleKey* RG_KeyHandlerRecordGet(Record* r){
    assert(r->type == KEY_HANDLER_RECORD);
    return r->keyHandlerRecord.keyHandler;
}

#ifdef WITHPYTHON
Record* RG_PyObjRecordCreate(){
    Record* ret = RG_ALLOC(sizeof(Record));
    ret->type = PY_RECORD;
    ret->pyRecord.obj = NULL;
    return ret;
}

PyObject* RG_PyObjRecordGet(Record* r){
    assert(r->type == PY_RECORD);
    return r->pyRecord.obj;
}

void RG_PyObjRecordSet(Record* r, PyObject* obj){
    assert(r->type == PY_RECORD);
    r->pyRecord.obj = obj;
}
#endif

void RG_SerializeRecord(BufferWriter* bw, Record* r){
    RedisGears_BWWriteLong(bw, r->type);
    switch(r->type){
    case STRING_RECORD:
        RedisGears_BWWriteBuffer(bw, r->stringRecord.str, r->stringRecord.len);
        break;
    case LONG_RECORD:
        RedisGears_BWWriteLong(bw, r->longRecord.num);
        break;
    case DOUBLE_RECORD:
        RedisGears_BWWriteLong(bw, (long)r->doubleRecord.num);
        break;
    case LIST_RECORD:
        RedisGears_BWWriteLong(bw, RedisGears_ListRecordLen(r));
        for(size_t i = 0 ; i < RedisGears_ListRecordLen(r) ; ++i){
            RG_SerializeRecord(bw, r->listRecord.records[i]);
        }
        break;
    case KEY_RECORD:
        RedisGears_BWWriteString(bw, r->keyRecord.key);
        if(r->keyRecord.record){
            RedisGears_BWWriteLong(bw, 1); // value exists
            RG_SerializeRecord(bw, r->keyRecord.record);
        }else{
            RedisGears_BWWriteLong(bw, 0); // value missing
        }
        break;
    case KEY_HANDLER_RECORD:
        assert(false && "can not serialize key handler record");
        break;
#ifdef WITHPYTHON
    case PY_RECORD:
        RedisGearsPy_PyObjectSerialize(r->pyRecord.obj, bw);
        break;
#endif
    default:
        assert(false);
    }
}

Record* RG_DeserializeRecord(BufferReader* br){
    enum RecordType type = RedisGears_BRReadLong(br);
    Record* r;
    char* temp;
    char* temp1;
    size_t size;
    switch(type){
    case STRING_RECORD:
        temp = RedisGears_BRReadBuffer(br, &size);
        temp1 = RG_ALLOC(size);
        memcpy(temp1, temp, size);
        r = RG_StringRecordCreate(temp1, size);
        break;
    case LONG_RECORD:
        r = RG_LongRecordCreate(RedisGears_BRReadLong(br));
        break;
    case DOUBLE_RECORD:
        r = RG_DoubleRecordCreate((double)RedisGears_BRReadLong(br));
        break;
    case LIST_RECORD:
        size = (size_t)RedisGears_BRReadLong(br);
        r = RG_ListRecordCreate(size);
        for(size_t i = 0 ; i < size ; ++i){
            RG_ListRecordAdd(r, RG_DeserializeRecord(br));
        }
        break;
    case KEY_RECORD:
        r = RedisGears_KeyRecordCreate();
        char* key = RG_STRDUP(RedisGears_BRReadString(br));
        RG_KeyRecordSetKey(r, key, strlen(key));
        bool isValExists = (bool)RedisGears_BRReadLong(br);
        if(isValExists){
            RedisGears_KeyRecordSetVal(r, RG_DeserializeRecord(br));
        }else{
            RedisGears_KeyRecordSetVal(r, NULL);
        }
        break;
    case KEY_HANDLER_RECORD:
        assert(false && "can not deserialize key handler record");
        break;
#ifdef WITHPYTHON
    case PY_RECORD:
        r = RG_PyObjRecordCreate();
        PyObject* obj = RedisGearsPy_PyObjectDeserialize(br);
        r->pyRecord.obj = obj;
        break;
#endif
    default:
        assert(false);
    }
    return r;
}

